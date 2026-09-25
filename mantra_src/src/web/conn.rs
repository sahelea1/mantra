//! One client connection: the hello handshake, commands into the App, outbound fan-out,
//! keep-alive and backpressure. `dispatch` is shared by local WebSockets (below) and relay clients
//! (`remote.rs`), so both transports speak exactly the same protocol.

use super::protocol::{ClientMsg, ServerMsg};
use super::{ConnId, ConnKind, Inbound, Outbound, WebRegistryHandle};
use crate::app::AppEvent;
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

/// A client must say hello within this long.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Keep-alive interval; two unanswered pings drop the connection.
pub const PING_EVERY: Duration = Duration::from_secs(25);

/// What the transport should do after one client message.
#[derive(Debug, PartialEq)]
pub enum Dispatched {
    Nothing,
    /// Send this text back right away (pong, or a refused command's result).
    Reply(String),
    /// The client answered a keep-alive ping.
    Pong,
}

/// Handle one text message from a client of either transport.
pub fn dispatch(reg: &WebRegistryHandle, tx: &UnboundedSender<AppEvent>, conn: ConnId, text: &str, hello_seen: &mut bool) -> Dispatched {
    match ClientMsg::parse(text) {
        Ok(ClientMsg::Hello { .. }) => {
            *hello_seen = true;
            reg.hello(conn);
            Dispatched::Nothing
        }
        Ok(ClientMsg::Ping) => Dispatched::Reply(ServerMsg::Pong.to_json()),
        Ok(ClientMsg::Pong) => Dispatched::Pong,
        Ok(ClientMsg::Cmd { req, cmd }) => {
            if !*hello_seen {
                return Dispatched::Reply(ServerMsg::err(req, "say hello first").to_json());
            }
            let _ = tx.send(AppEvent::Web(Inbound { conn, req, cmd }));
            Dispatched::Nothing
        }
        Err(e) => match e.req {
            Some(req) => Dispatched::Reply(ServerMsg::err(req, e.error).to_json()),
            None => {
                crate::mlog!("web: conn {conn}: {}", e.error);
                Dispatched::Nothing
            }
        },
    }
}

/// A local (same machine / LAN) WebSocket, already authenticated by the server.
pub async fn run_local(socket: WebSocket, reg: WebRegistryHandle, tx: UnboundedSender<AppEvent>) {
    let mut h = reg.register(ConnKind::Local);
    let id = h.id;
    let (mut sink, mut stream) = socket.split();
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let hello_deadline = tokio::time::sleep(HELLO_TIMEOUT);
    tokio::pin!(hello_deadline);
    let mut hello_seen = false;
    let mut missed = 0u8;
    loop {
        tokio::select! {
            m = stream.next() => match m {
                Some(Ok(Message::Text(t))) => match dispatch(&reg, &tx, id, t.as_str(), &mut hello_seen) {
                    Dispatched::Reply(r) => {
                        if sink.send(Message::Text(r.into())).await.is_err() {
                            break;
                        }
                    }
                    Dispatched::Pong => missed = 0,
                    Dispatched::Nothing => {}
                },
                Some(Ok(Message::Pong(_))) => missed = 0,
                Some(Ok(Message::Ping(_))) | Some(Ok(Message::Binary(_))) => {}
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
            },
            o = h.recv() => match o {
                Some(Outbound::Text(t)) => {
                    if sink.send(Message::Text(t.as_ref().into())).await.is_err() {
                        break;
                    }
                }
                Some(Outbound::Close(reason)) => {
                    let _ = sink.send(Message::Text(ServerMsg::Bye { reason: reason.into() }.to_json().into())).await;
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
                None => break,
            },
            _ = ping.tick() => {
                if missed >= 2 {
                    break;
                }
                missed += 1;
                if sink.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
            }
            _ = &mut hello_deadline, if !hello_seen => break,
        }
    }
    reg.unregister(id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_need_a_hello_and_go_to_the_app() {
        let (ctl, mut ctl_rx) = tokio::sync::mpsc::unbounded_channel();
        let reg = WebRegistryHandle::new(ctl);
        let h = reg.register(ConnKind::Local);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut hello = false;
        let cmd = r#"{"t":"cmd","req":3,"cmd":"land"}"#;
        assert!(matches!(dispatch(&reg, &tx, h.id, cmd, &mut hello), Dispatched::Reply(r) if r.contains("say hello first")));
        assert_eq!(dispatch(&reg, &tx, h.id, r#"{"t":"hello","protocol":1}"#, &mut hello), Dispatched::Nothing);
        assert!(hello && matches!(ctl_rx.try_recv(), Ok(super::super::Control::Hello { conn }) if conn == h.id));
        assert_eq!(dispatch(&reg, &tx, h.id, cmd, &mut hello), Dispatched::Nothing);
        assert!(matches!(rx.try_recv(), Ok(AppEvent::Web(Inbound { req: 3, .. }))));
        assert_eq!(dispatch(&reg, &tx, h.id, r#"{"t":"ping"}"#, &mut hello), Dispatched::Reply(r#"{"t":"pong"}"#.into()));
        assert_eq!(dispatch(&reg, &tx, h.id, r#"{"t":"pong"}"#, &mut hello), Dispatched::Pong);
        assert!(matches!(dispatch(&reg, &tx, h.id, r#"{"t":"cmd","req":4,"cmd":"rm_rf"}"#, &mut hello), Dispatched::Reply(r) if r.contains("unknown command rm_rf") && r.contains("\"req\":4")));
    }
}
