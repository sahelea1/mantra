//! The optional control socket (SPEC §4.5): `<session dir>/control.sock`, ops in, events fanned out.
//!
//! Each connection is two independent halves. Lines read from it are ops, fed into the very inbox
//! stdin feeds, so the agent cannot tell (and need not care) where an op came from. Every event the
//! session emits from the moment of connecting is queued to it, one per line, until it goes away. A
//! client's queue is registered *before* its first op is read, so `bija send` always sees the answer
//! to what it sent. A client's queue is bounded: one that stops reading while it stays connected is
//! dropped once its queue is full, instead of growing bija's memory with every delta. The socket is
//! private to the user (0600); the session lock makes any socket file found at start a leftover of
//! a dead process, so it is replaced.

use crate::agent::Input;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{self, Receiver, Sender, UnboundedSender};
use tokio::task::JoinHandle;

pub const SOCKET_FILE: &str = "control.sock";

/// Event lines queued per client before it counts as gone (a few seconds of streaming deltas).
const CLIENT_QUEUE: usize = 1024;

pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join(SOCKET_FILE)
}

/// The event queues of every connected client. Cloned into the agent's event sink.
#[derive(Clone, Default)]
pub struct Fanout {
    clients: Arc<Mutex<Vec<Sender<Arc<str>>>>>,
}

impl Fanout {
    /// Queues one event line to every client, dropping the ones that went away or stopped reading.
    pub fn send(&self, line: &str) {
        let Ok(mut clients) = self.clients.lock() else {
            return;
        };
        if clients.is_empty() {
            return;
        }
        let line: Arc<str> = Arc::from(line);
        clients.retain(|c| c.try_send(line.clone()).is_ok());
    }

    /// A new client queue that receives every line sent from now on.
    pub fn subscribe(&self) -> Receiver<Arc<str>> {
        let (tx, rx) = mpsc::channel(CLIENT_QUEUE);
        if let Ok(mut clients) = self.clients.lock() {
            clients.push(tx);
        }
        rx
    }
}

/// A listening control socket. Dropping it stops accepting and removes the socket file.
pub struct Control {
    path: PathBuf,
    fanout: Fanout,
    accept: JoinHandle<()>,
}

impl Control {
    /// Binds `<dir>/control.sock` (replacing a stale file) and starts accepting. Must run inside the
    /// runtime.
    pub fn start(dir: &Path, inbox: UnboundedSender<Input>) -> io::Result<Control> {
        let path = socket_path(dir);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let fanout = Fanout::default();
        let accept = tokio::spawn(accept_loop(listener, fanout.clone(), inbox));
        Ok(Control { path, fanout, accept })
    }

    pub fn fanout(&self) -> Fanout {
        self.fanout.clone()
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        self.accept.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn accept_loop(listener: UnixListener, fanout: Fanout, inbox: UnboundedSender<Input>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => attach(stream, &fanout, &inbox),
            Err(e) => {
                // Out of descriptors and the like: back off instead of spinning on the error.
                eprintln!("bija: control socket: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

fn attach(stream: UnixStream, fanout: &Fanout, inbox: &UnboundedSender<Input>) {
    let (read, write) = stream.into_split();
    let lines = fanout.subscribe();
    tokio::spawn(write_events(write, lines));
    tokio::spawn(read_ops(read, inbox.clone()));
}

async fn write_events(mut w: OwnedWriteHalf, mut lines: Receiver<Arc<str>>) {
    let mut buf = Vec::new();
    while let Some(line) = lines.recv().await {
        buf.clear();
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        if w.write_all(&buf).await.is_err() {
            return;
        }
    }
}

/// Feeds the client's lines into the inbox. A client closing its side is not a shutdown: only stdin
/// EOF is.
async fn read_ops(r: OwnedReadHalf, inbox: UnboundedSender<Input>) {
    let mut reader = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                if let Some(input) = Input::from_line(&String::from_utf8_lossy(&buf)) {
                    if inbox.send(input).is_err() {
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Op;
    use crate::types::uuid_v4;

    fn scratch() -> PathBuf {
        let p = std::env::temp_dir().join(format!("bija-ctl-{}", &uuid_v4()[..8]));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn fanout_reaches_every_live_client_and_forgets_the_dead() {
        let f = Fanout::default();
        f.send("nobody listens");
        let mut a = f.subscribe();
        let b = f.subscribe();
        drop(b);
        f.send("one");
        assert_eq!(a.try_recv().unwrap().as_ref(), "one");
        assert_eq!(f.clients.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_client_that_stops_reading_is_dropped_once_its_queue_is_full() {
        let f = Fanout::default();
        let mut slow = f.subscribe();
        for i in 0..CLIENT_QUEUE {
            f.send(&format!("line {i}"));
        }
        assert_eq!(f.clients.lock().unwrap().len(), 1, "a full queue is still a client");
        f.send("one too many");
        assert!(f.clients.lock().unwrap().is_empty(), "the overflow drops it");
        // What it had queued is still there to read; then its queue ends.
        assert_eq!(slow.try_recv().unwrap().as_ref(), "line 0");
        let mut left = 1;
        while slow.try_recv().is_ok() {
            left += 1;
        }
        assert_eq!(left, CLIENT_QUEUE);
    }

    #[tokio::test]
    async fn ops_come_in_and_events_go_out() {
        let dir = scratch();
        // A stale socket file from a dead process is replaced.
        std::fs::write(socket_path(&dir), "stale").unwrap();
        let (tx, mut inbox) = mpsc::unbounded_channel();
        let ctl = Control::start(&dir, tx).unwrap();
        let path = socket_path(&dir);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        let stream = UnixStream::connect(&path).await.unwrap();
        let (r, mut w) = stream.into_split();
        w.write_all(b"{\"op\":\"status\"}\nnot an op\n").await.unwrap();
        match inbox.recv().await {
            Some(Input::Op(Op::Status)) => {}
            _ => panic!("expected the status op"),
        }
        match inbox.recv().await {
            Some(Input::Bad(m)) => assert!(m.starts_with("bad op"), "{m}"),
            _ => panic!("expected a bad-op input"),
        }
        // The client was registered before its first op was read, so it sees what follows.
        ctl.fanout().send(r#"{"ev":"status"}"#);
        let mut lines = BufReader::new(r).lines();
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some(r#"{"ev":"status"}"#));

        drop(ctl);
        assert!(!path.exists(), "the socket file is removed with the listener");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
