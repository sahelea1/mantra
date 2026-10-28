//! Conservative cleanup of abandoned PID-named MCP bridge sockets.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

/// Only remove sockets whose named process is positively gone and whose endpoint refuses a
/// connection. Unknown errors, reused/live PIDs, symlinks, foreign owners, and other files stay.
pub(super) fn clean_dead_sockets(dir: &Path) {
    clean_with(dir, process_is_gone);
}

fn process_is_gone(pid: i32) -> bool {
    let status = unsafe { libc::kill(pid, 0) };
    process_probe_is_gone(status, std::io::Error::last_os_error().raw_os_error())
}

fn process_probe_is_gone(status: i32, errno: Option<i32>) -> bool {
    status == -1 && errno == Some(libc::ESRCH)
}

fn clean_with(dir: &Path, mut gone: impl FnMut(i32) -> bool) {
    let Ok(directory) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)
    else {
        return;
    };
    let Ok(owner) = directory.metadata() else {
        return;
    };
    let uid = unsafe { libc::geteuid() };
    // A writable directory controlled by someone else cannot safely be swept.
    if owner.uid() != uid || owner.mode() & 0o022 != 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let same_directory = || {
        std::fs::symlink_metadata(dir)
            .is_ok_and(|m| m.is_dir() && m.dev() == owner.dev() && m.ino() == owner.ino())
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = socket_pid(name.as_bytes()) else {
            continue;
        };
        let Ok(name) = CString::new(name.as_bytes()) else {
            continue;
        };
        let stat = || {
            let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    metadata.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } == 0
            {
                Some(unsafe { metadata.assume_init() })
            } else {
                None
            }
        };
        let Some(before) = stat() else {
            continue;
        };
        if before.st_mode & libc::S_IFMT != libc::S_IFSOCK || before.st_uid != uid || !gone(pid) {
            continue;
        }
        if !same_directory()
            || !connection_refused(&entry.path())
            || !same_directory()
            || !gone(pid)
        {
            continue;
        }
        let Some(after) = stat() else {
            continue;
        };
        if before.st_dev == after.st_dev
            && before.st_ino == after.st_ino
            && after.st_mode & libc::S_IFMT == libc::S_IFSOCK
            && after.st_uid == uid
        {
            // Anchor removal to the inspected directory, even if its pathname is replaced.
            unsafe {
                libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0);
            }
        }
    }
}

fn socket_pid(name: &[u8]) -> Option<i32> {
    let digits = name.strip_suffix(b".sock")?;
    if digits.is_empty() || digits[0] == b'0' || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let pid: i32 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    (pid > 0).then_some(pid)
}

/// Probe without blocking when a live server's listen queue is full. Only ECONNREFUSED proves
/// an existing socket has no listener; permissions and all ambiguous errors preserve it.
fn connection_refused(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return false;
    }
    address.sun_family = libc::AF_UNIX as _;
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        address.sun_len = std::mem::size_of::<libc::sockaddr_un>() as _;
    }
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as _;
    }
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return false;
    }
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };
    if unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1
        || unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } == -1
    {
        return false;
    }
    let status = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of_val(&address) as _,
        )
    };
    status == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECONNREFUSED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::os::unix::net::UnixListener;

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("mantra-sock-{}", crate::util::uuid_v4()));
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
        fn abandoned(&self, name: &str) -> std::path::PathBuf {
            let path = self.0.join(name);
            drop(UnixListener::bind(&path).unwrap());
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn removes_only_dead_pid_sockets_and_preserves_live_endpoints() {
        let fixture = Fixture::new();
        let stale = fixture.abandoned("20001.sock");
        let live_pid = fixture.abandoned("20002.sock");
        let live = fixture.0.join("20003.sock");
        let _listener = UnixListener::bind(&live).unwrap();
        let unrelated = fixture.abandoned("service.sock");
        let padded = fixture.abandoned("020001.sock");
        let regular = fixture.0.join("20004.sock");
        std::fs::write(&regular, "keep").unwrap();
        let link = fixture.0.join("20005.sock");
        symlink(&stale, &link).unwrap();
        clean_with(&fixture.0, |pid| pid != 20002);
        assert!(!stale.exists());
        for path in [&live_pid, &live, &unrelated, &padded, &regular] {
            assert!(path.exists(), "{}", path.display());
        }
        assert!(std::fs::symlink_metadata(link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn does_not_follow_directory_symlinks_or_sweep_shared_writable_directories() {
        let fixture = Fixture::new();
        let stale = fixture.abandoned("20001.sock");
        let link = fixture.0.with_extension("link");
        symlink(&fixture.0, &link).unwrap();
        clean_with(&link, |_| true);
        std::fs::remove_file(link).unwrap();
        assert!(stale.exists());
        std::fs::set_permissions(&fixture.0, std::fs::Permissions::from_mode(0o777)).unwrap();
        clean_with(&fixture.0, |_| true);
        assert!(stale.exists());
    }

    #[test]
    fn only_esrch_proves_process_is_gone() {
        assert!(!process_is_gone(std::process::id() as i32));
        assert!(process_probe_is_gone(-1, Some(libc::ESRCH)));
        assert!(!process_probe_is_gone(-1, Some(libc::EPERM)));
        assert!(!process_probe_is_gone(-1, Some(libc::EIO)));
        assert!(!process_probe_is_gone(0, Some(libc::ESRCH)));
    }
}
