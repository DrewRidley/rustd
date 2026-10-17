//! The sockets the daemon and its workers take requests on. One is for its user alone: it is
//! in a directory only the user can enter, only the user can connect to it, and a peer that
//! is another user is turned away. One process serves it: whoever holds the lock next to
//! it. A request is one line, read with a limit on its length and on the time it may take,
//! by a thread of its own, so a client that says nothing holds up no other.

use std::{
    fs::File,
    io::{self, BufRead as _, BufReader, Read as _},
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(windows)]
use uds_windows::{UnixListener, UnixStream};

/// The longest request: a build names every file rustc linked and every source it read.
pub(crate) const LONGEST_REQUEST: u64 = 16 << 20;

/// How long a client has to send its request once it is connected.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A socket this process alone serves, for as long as this is kept.
pub(crate) struct Served {
    pub(crate) listener: UnixListener,
    _lock: File,
}

/// Why a socket is not served by this process.
#[derive(Debug)]
pub(crate) enum NotServed {
    /// Another process holds the lock and serves it.
    Taken,
    Failed(io::Error),
}

impl From<io::Error> for NotServed {
    fn from(error: io::Error) -> NotServed {
        NotServed::Failed(error)
    }
}

impl Served {
    /// Listens on `socket`. The lock next to it is taken first, so of two processes that
    /// start together one listens and the other is told the socket is taken; a socket file
    /// that is there once the lock is held was left by a process that is gone.
    pub(crate) fn bind(socket: &Path) -> Result<Served, NotServed> {
        let lock = File::create(lock_of(socket))?;
        if lock.try_lock().is_err() {
            return Err(NotServed::Taken);
        }
        match std::fs::remove_file(socket) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        let listener = UnixListener::bind(socket)?;
        only_the_user_connects(socket)?;
        Ok(Served { listener, _lock: lock })
    }
}

fn lock_of(socket: &Path) -> PathBuf {
    let mut lock = socket.as_os_str().to_owned();
    lock.push(".lock");
    PathBuf::from(lock)
}

/// Makes `dir` if it is not there, for the user alone, and refuses one that is someone
/// else's or that others can enter: whoever can enter it can put a socket of their own in
/// the place of the daemon's.
#[cfg(unix)]
pub(crate) fn private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
    let made = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
    made?;
    let is = std::fs::symlink_metadata(dir)?;
    let refused = |why: &str| {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} {why}", dir.display())))
    };
    if !is.is_dir() {
        return refused("is not a directory");
    }
    if is.uid() != user() {
        return refused("belongs to another user");
    }
    if is.mode() & 0o077 != 0 {
        return refused("can be entered by other users (`chmod 700` it)");
    }
    Ok(())
}

/// A directory of the user's profile is the user's by its access control list, which a
/// directory made in it inherits.
#[cfg(windows)]
pub(crate) fn private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn only_the_user_connects(socket: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
fn only_the_user_connects(_: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn user() -> u32 {
    // SAFETY: `geteuid` takes nothing and can't fail.
    unsafe { libc::geteuid() }
}

/// The user at the other end of `stream`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_user(stream: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd as _;
    let mut peer = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `peer` is a `ucred` and `len` its size, which is what `SO_PEERCRED` writes.
    let failed = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut peer).cast(),
            &raw mut len,
        )
    };
    if failed != 0 { Err(io::Error::last_os_error()) } else { Ok(peer.uid) }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn peer_user(stream: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd as _;
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: both point at integers of the types `getpeereid` writes.
    let failed = unsafe { libc::getpeereid(stream.as_raw_fd(), &raw mut uid, &raw mut gid) };
    if failed != 0 { Err(io::Error::last_os_error()) } else { Ok(uid) }
}

/// Turns away a peer that is another user.
#[cfg(unix)]
fn check_peer(stream: &UnixStream) -> io::Result<()> {
    let peer = peer_user(stream)?;
    if peer == user() {
        return Ok(());
    }
    Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("the peer is user {peer}")))
}

/// Windows has no user of a peer to ask for: the socket is in the user's profile, which
/// other users can't open.
#[cfg(windows)]
fn check_peer(_: &UnixStream) -> io::Result<()> {
    Ok(())
}

/// Reads the one line a client sends, without its end: of the user, within the time and
/// the length a request may take.
pub(crate) fn read_request(stream: &UnixStream) -> io::Result<String> {
    let [request] = read_lines_within(stream, LONGEST_REQUEST, REQUEST_TIMEOUT)?;
    Ok(request)
}

/// Reads the two lines a client of a worker sends, as [`read_request`] does one: the
/// request, and the variables of the client's process.
pub(crate) fn read_request_and_env(stream: &UnixStream) -> io::Result<[String; 2]> {
    read_lines_within(stream, LONGEST_REQUEST, REQUEST_TIMEOUT)
}

fn read_lines_within<const LINES: usize>(
    stream: &UnixStream,
    longest: u64,
    timeout: Duration,
) -> io::Result<[String; LINES]> {
    check_peer(stream)?;
    if let Err(e) = stream.set_read_timeout(Some(timeout))
        && e.kind() != io::ErrorKind::InvalidInput
    {
        return Err(e);
    }
    let mut lines = BufReader::new(stream.take(longest));
    let mut read = [const { String::new() }; LINES];
    for line in &mut read {
        *line = read_line(&mut lines)?;
    }
    // What a build takes is not the client's to be timed for.
    let _ = stream.set_read_timeout(None);
    Ok(read)
}

/// The next line of `lines` without its end, when it has one before `lines` is at its limit.
fn read_line(lines: &mut BufReader<io::Take<&UnixStream>>) -> io::Result<String> {
    let mut line = String::new();
    lines.read_line(&mut line)?;
    match line.strip_suffix('\n') {
        Some(line) => Ok(line.to_owned()),
        None if lines.get_ref().limit() == 0 => {
            Err(io::Error::new(io::ErrorKind::InvalidData, "the request is too long"))
        }
        None => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the request has no end")),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{io::Write as _, os::unix::fs::PermissionsExt as _, time::Instant};

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rustd-socket-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_directory_and_the_socket_are_the_users_alone() {
        let dir = scratch("private");
        private_dir(&dir).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        let served = Served::bind(&dir.join("sock")).unwrap();
        assert_eq!(
            std::fs::metadata(dir.join("sock")).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // The peer of a connection from this process is this user.
        let _client = UnixStream::connect(dir.join("sock")).unwrap();
        let (peer, _) = served.listener.accept().unwrap();
        assert_eq!(peer_user(&peer).unwrap(), user());
        check_peer(&peer).unwrap();

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let refused = private_dir(&dir).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        let file = dir.join("file");
        std::fs::write(&file, "").unwrap();
        assert_eq!(private_dir(&file).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_silent_or_endless_client_is_given_up_on() {
        let dir = scratch("requests");
        private_dir(&dir).unwrap();
        let served = Served::bind(&dir.join("sock")).unwrap();
        let connect = || {
            let client = UnixStream::connect(dir.join("sock")).unwrap();
            (client, served.listener.accept().unwrap().0)
        };
        let short = Duration::from_millis(100);

        let (_silent, stream) = connect();
        let began = Instant::now();
        let timed_out = read_lines_within::<1>(&stream, 64, short).unwrap_err();
        assert!(matches!(timed_out.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut));
        assert!(began.elapsed() < Duration::from_secs(5));

        let (mut endless, stream) = connect();
        endless.write_all(&[b'a'; 200]).unwrap();
        let too_long = read_lines_within::<1>(&stream, 64, short).unwrap_err();
        assert_eq!(too_long.kind(), io::ErrorKind::InvalidData);

        let (mut cut_short, stream) = connect();
        cut_short.write_all(b"sync").unwrap();
        drop(cut_short);
        let no_end = read_lines_within::<1>(&stream, 64, short).unwrap_err();
        assert_eq!(no_end.kind(), io::ErrorKind::UnexpectedEof);

        // The others did not keep this one from being heard.
        let (mut good, stream) = connect();
        good.write_all(b"sync\nA=b\n").unwrap();
        let heard: [String; 2] = read_lines_within(&stream, 64, short).unwrap();
        assert_eq!(heard, ["sync", "A=b"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn one_process_serves_a_socket() {
        let dir = scratch("lock");
        private_dir(&dir).unwrap();
        let socket = dir.join("sock");
        let first = Served::bind(&socket).unwrap();
        // The second leaves the socket of the first where it is.
        assert!(matches!(Served::bind(&socket), Err(NotServed::Taken)));
        assert!(UnixStream::connect(&socket).is_ok());
        drop(first);
        // The file a process left behind is no socket anyone serves, and is replaced.
        assert!(socket.exists());
        let second = Served::bind(&socket).unwrap();
        assert!(UnixStream::connect(&socket).is_ok());
        drop(second);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
