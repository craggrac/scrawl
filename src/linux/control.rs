//! The running `scrawl app`'s control socket, `$XDG_RUNTIME_DIR/scrawl/ctl.sock`: one command
//! per connection, a line in and a line back (`ok` or `error: …`). `scrawl toggle` uses it
//! when an app is running, so the Hyprland bind drives the app; it is also what keeps a
//! second `scrawl app` from starting. The directory is the owner-only one the recorder's
//! pidfile uses, and a peer must be the same user.

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::toggle::{create_runtime_dir, runtime_dir};
use crate::capture::scrawl_log;

/// What the socket accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// The capture shortcut, as the key bind presses it.
    Toggle,
    /// Finish the recording, if one is running.
    Stop,
    /// Throw the recording or the picker away.
    Cancel,
    /// Open the review window on the latest capture.
    Review,
    Quit,
}

impl Command {
    pub fn word(self) -> &'static str {
        match self {
            Self::Toggle => "toggle",
            Self::Stop => "stop",
            Self::Cancel => "cancel",
            Self::Review => "review",
            Self::Quit => "quit",
        }
    }
}

/// One request line (`"toggle\n"`); `None` for anything else.
pub fn parse(line: &str) -> Option<Command> {
    [
        Command::Toggle,
        Command::Stop,
        Command::Cancel,
        Command::Review,
        Command::Quit,
    ]
    .into_iter()
    .find(|command| command.word() == line.trim())
}

/// What to do about a socket path a new app wants to bind.
#[derive(Debug, PartialEq, Eq)]
pub enum Existing {
    /// Nothing there: bind.
    Free,
    /// A socket nobody answers on (an app that crashed or was killed): replace it.
    Stale,
    /// An app answers: this one must not start.
    Live,
}

/// From connecting to the path: `Ok` is a live app, `NotFound` a free path,
/// `ConnectionRefused` a stale socket. Anything else is not ours to remove.
pub fn existing(connect: &std::io::Result<()>) -> Result<Existing> {
    match connect {
        Ok(()) => Ok(Existing::Live),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Existing::Free),
        Err(error) if error.kind() == ErrorKind::ConnectionRefused => Ok(Existing::Stale),
        Err(error) => bail!("Could not check Scrawl's control socket: {error}"),
    }
}

/// The socket's path.
pub fn socket_path() -> Option<PathBuf> {
    runtime_dir().map(|dir| dir.join("ctl.sock"))
}

/// How long either side waits on the other.
const TIMEOUT: Duration = Duration::from_secs(2);

/// The directory must be a real directory of ours that nobody else can enter.
fn check_private(dir: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(dir)
        .with_context(|| format!("Could not read {}", dir.display()))?;
    let uid = unsafe { libc::geteuid() };
    if !meta.is_dir() || meta.uid() != uid || meta.permissions().mode() & 0o077 != 0 {
        bail!(
            "{} must be a directory only you can open (0700); not using it",
            dir.display()
        );
    }
    Ok(())
}

/// Binds the socket for this app: a live app there stops this one; a stale socket is
/// replaced.
pub fn bind() -> Result<(UnixListener, PathBuf)> {
    let path = socket_path().context("No per-user runtime or state directory")?;
    let dir = path.parent().context("The socket path has no directory")?;
    create_runtime_dir(dir)?;
    check_private(dir)?;
    match existing(&UnixStream::connect(&path).map(drop))? {
        Existing::Live => bail!(
            "Scrawl is already running (it answers on {}): `scrawl toggle` records with it, and \
             Quit Scrawl in its tray menu ends it",
            path.display()
        ),
        Existing::Stale => {
            scrawl_log(format!(
                "Replacing a stale control socket at {}",
                path.display()
            ));
            std::fs::remove_file(&path)
                .with_context(|| format!("Could not remove {}", path.display()))?;
        }
        Existing::Free => {}
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("Could not listen on {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok((listener, path))
}

/// The uid of the process at the other end.
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

/// Answers requests on a thread for as long as the app runs. `handle` says what came of a
/// command; its error goes back to the sender.
pub fn serve(
    listener: UnixListener,
    handle: impl Fn(Command) -> std::result::Result<(), String> + Send + 'static,
) {
    let spawned = std::thread::Builder::new()
        .name("scrawl-control".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => answer(stream, &handle),
                    Err(error) => scrawl_log(format!("Control socket: {error}")),
                }
            }
        });
    if let Err(error) = spawned {
        scrawl_log(format!(
            "Control socket: could not start its thread: {error}"
        ));
    }
}

fn answer(stream: UnixStream, handle: &dyn Fn(Command) -> std::result::Result<(), String>) {
    if peer_uid(&stream) != Some(unsafe { libc::geteuid() }) {
        scrawl_log("Control socket: refused a peer that isn't this user");
        return;
    }
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    let _ = stream.set_write_timeout(Some(TIMEOUT));
    let mut line = String::new();
    // A command is one short word; nothing longer is read.
    let read = BufReader::new(std::io::Read::take(&stream, 64)).read_line(&mut line);
    let reply = match read.ok().and_then(|_| parse(&line)) {
        Some(command) => match handle(command) {
            Ok(()) => "ok".to_owned(),
            Err(error) => format!("error: {error}"),
        },
        None => "error: unknown command (toggle, stop, cancel, review, quit)".to_owned(),
    };
    let _ = writeln!(&stream, "{reply}");
}

/// Sends `command` to the running app; `false` when no app is listening.
pub fn send(command: Command) -> Result<bool> {
    let Some(path) = socket_path() else {
        return Ok(false);
    };
    let stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    };
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    writeln!(&stream, "{}", command.word())?;
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply)?;
    match reply.trim() {
        "ok" => Ok(true),
        "" => bail!("Scrawl closed the control socket without answering"),
        other => bail!("Scrawl: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_one_known_word_per_line() {
        for command in [
            Command::Toggle,
            Command::Stop,
            Command::Cancel,
            Command::Review,
            Command::Quit,
        ] {
            assert_eq!(parse(&format!("{}\n", command.word())), Some(command));
        }
        assert_eq!(parse("  toggle \r\n"), Some(Command::Toggle));
        assert_eq!(parse("TOGGLE"), None);
        assert_eq!(parse("toggle now"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn only_a_refused_socket_is_replaced() {
        use std::io::Error;
        assert_eq!(existing(&Ok(())).unwrap(), Existing::Live);
        assert_eq!(
            existing(&Err(Error::from(ErrorKind::NotFound))).unwrap(),
            Existing::Free
        );
        assert_eq!(
            existing(&Err(Error::from(ErrorKind::ConnectionRefused))).unwrap(),
            Existing::Stale
        );
        assert!(existing(&Err(Error::from(ErrorKind::PermissionDenied))).is_err());
    }
}
