//! Listening sockets a supervisor passed to `serve` as open descriptors,
//! under the protocol of systemd's socket activation (`sd_listen_fds(3)`):
//! descriptors start at 3, `LISTEN_FDS` is how many there are,
//! `LISTEN_FDNAMES` names them in order, separated by `:`, and `LISTEN_PID`
//! is the process they were passed to.
//!
//! The `LISTEN_*` variables stay in the environment. `LISTEN_PID` names this
//! process, so no child takes them for its own, and removing them needs
//! `std::env::remove_var`, which is unsafe once the runtime's threads run.

use std::collections::BTreeSet;
use std::os::fd::{FromRawFd as _, RawFd};
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Context as _;

/// The first descriptor passed under the protocol.
const FIRST: RawFd = 3;

/// The descriptors [`take`] has handed out. A descriptor has one owner, so a
/// second take of one is refused rather than handed to a second listener,
/// which would close it twice.
static TAKEN: Mutex<BTreeSet<RawFd>> = Mutex::new(BTreeSet::new());

/// A name is 1 to 255 characters, with no `:`, which separates names in
/// `LISTEN_FDNAMES`, and no control characters.
pub fn name_is_valid(name: &str) -> bool {
    (1..=255).contains(&name.chars().count()) && !name.chars().any(|c| c == ':' || c.is_control())
}

/// How many descriptors were passed to this process.
fn count() -> anyhow::Result<RawFd> {
    let pid = std::process::id();
    match std::env::var("LISTEN_PID") {
        Err(_) => anyhow::bail!("LISTEN_PID is not set"),
        Ok(listen_pid) if listen_pid.parse() != Ok(pid) => {
            anyhow::bail!("LISTEN_PID {listen_pid:?} names another process; this one is {pid}")
        }
        Ok(_) => {}
    }
    let fds = std::env::var("LISTEN_FDS").context("LISTEN_FDS is not set")?;
    fds.parse::<RawFd>()
        .ok()
        .filter(|n| (0..=RawFd::MAX - FIRST).contains(n))
        .with_context(|| format!("LISTEN_FDS {fds:?} is not a number of descriptors"))
}

/// Marks every descriptor passed to this process close-on-exec, named or
/// not, so that no git process the world starts holds one. A socket a git
/// process holds outlives the world, and anything that process runs can
/// accept the connections meant for the world's next `serve`.
pub fn close_on_exec() {
    let Ok(count) = count() else {
        return;
    };
    for fd in FIRST..FIRST + count {
        // SAFETY: fcntl with F_GETFD and F_SETFD takes no pointers and
        // changes only the descriptor's flags.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            // A descriptor that is not open has nothing to mark.
            if flags != -1 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

/// Takes the passed socket named `name`, and returns it with its path. It is
/// never bound, unlinked, or replaced here: the supervisor made it, and
/// removes it.
pub fn take(name: &str) -> anyhow::Result<(tokio::net::UnixListener, PathBuf)> {
    let count = count()?;
    let names = std::env::var("LISTEN_FDNAMES").context("LISTEN_FDNAMES is not set")?;
    let mut positions = names
        .split(':')
        .enumerate()
        .filter(|&(_, n)| n == name)
        .map(|(i, _)| i);
    let position = match (positions.next(), positions.next()) {
        (Some(i), None) => i,
        (None, _) => anyhow::bail!("LISTEN_FDNAMES {names:?} does not name it"),
        (Some(_), Some(_)) => {
            anyhow::bail!("LISTEN_FDNAMES {names:?} names more than one descriptor {name:?}")
        }
    };
    let fd = RawFd::try_from(position)
        .ok()
        .filter(|&i| i < count)
        .map(|i| FIRST + i)
        .with_context(|| {
            format!(
                "LISTEN_FDNAMES names it in position {}, but LISTEN_FDS is {count}",
                position + 1
            )
        })?;
    check_listening_unix_stream(fd)?;
    let first_take = TAKEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(fd);
    anyhow::ensure!(first_take, "descriptor {fd} is already another listener's");
    // SAFETY: the descriptor was passed to this process to listen on, the
    // check above found it a listening Unix stream socket, and `TAKEN` makes
    // this its only owner.
    let listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(fd) };
    let addr = listener
        .local_addr()
        .with_context(|| format!("reading descriptor {fd}'s address"))?;
    let path = addr
        .as_pathname()
        .with_context(|| format!("descriptor {fd} is a socket with no path"))?
        .to_path_buf();
    // The ready line names the socket by its path, and a relative one means
    // nothing to a caller in another directory.
    anyhow::ensure!(
        path.is_absolute(),
        "descriptor {fd} is bound to {}, which is not an absolute path",
        path.display()
    );
    listener
        .set_nonblocking(true)
        .with_context(|| format!("making descriptor {fd} non-blocking"))?;
    let listener = tokio::net::UnixListener::from_std(listener)?;
    Ok((listener, path))
}

fn check_listening_unix_stream(fd: RawFd) -> anyhow::Result<()> {
    let option = |name| -> std::io::Result<libc::c_int> {
        let mut value: libc::c_int = 0;
        let mut len = size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `value` and `len` outlive the call, and `len` is the size
        // of `value`.
        let got = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                name,
                (&raw mut value).cast(),
                &mut len,
            )
        };
        if got == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(value)
    };
    let kind =
        option(libc::SO_TYPE).with_context(|| format!("descriptor {fd} is not an open socket"))?;
    anyhow::ensure!(
        kind == libc::SOCK_STREAM,
        "descriptor {fd} is not a stream socket"
    );
    let listening = option(libc::SO_ACCEPTCONN)
        .with_context(|| format!("reading whether descriptor {fd} listens"))?;
    anyhow::ensure!(listening != 0, "descriptor {fd} is not listening");

    let mut addr = std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: `addr` and `len` outlive the call, and `len` is the size of
    // `addr`.
    if unsafe { libc::getsockname(fd, addr.as_mut_ptr().cast(), &mut len) } == -1 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("reading descriptor {fd}'s address"));
    }
    // SAFETY: all zeroes is a valid sockaddr_storage, and getsockname wrote
    // no more than its size.
    let family = unsafe { addr.assume_init() }.ss_family;
    anyhow::ensure!(
        libc::c_int::from(family) == libc::AF_UNIX,
        "descriptor {fd} is not a Unix socket"
    );
    Ok(())
}
