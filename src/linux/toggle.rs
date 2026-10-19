//! `scrawl toggle`: one command for a Hyprland key bind that starts a recording, or stops the
//! one running. The running recorder's pid lives in `$XDG_RUNTIME_DIR/scrawl/record.pid`.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// What a key press should do.
#[derive(Debug, PartialEq, Eq)]
pub enum Toggle {
    Stop(i32),
    Start,
}

/// Where the running recorder's pid is kept: a per-user directory, never the shared temp dir.
pub fn pidfile() -> Option<PathBuf> {
    dirs::runtime_dir()
        .or_else(dirs::state_dir)
        .map(|dir| dir.join("scrawl/record.pid"))
}

/// Stop the pid in the file if `is_recorder` says it is still our recorder; else start one.
pub fn decide(contents: Option<&str>, is_recorder: impl Fn(i32) -> bool) -> Toggle {
    match contents.and_then(|c| c.trim().parse::<i32>().ok()) {
        Some(pid) if pid > 0 && is_recorder(pid) => Toggle::Stop(pid),
        _ => Toggle::Start,
    }
}

/// A recycled pid must never get our SIGINT: check it is `scrawl record --until-stopped`.
fn is_recorder(pid: i32) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| {
        let args: Vec<&[u8]> = cmdline.split(|&b| b == 0).collect();
        args.contains(&b"record".as_slice()) && args.contains(&b"--until-stopped".as_slice())
    })
}

/// Start a recording, or stop the running one.
pub fn toggle() -> Result<()> {
    let Some(path) = pidfile() else {
        bail!("No per-user runtime or state directory for the pidfile");
    };
    match decide(std::fs::read_to_string(&path).ok().as_deref(), is_recorder) {
        Toggle::Stop(pid) => {
            unsafe {
                libc::kill(pid, libc::SIGINT);
            }
            // The recorder clears the file itself; removing it now lets a quick third press
            // start a new recording while this one is still being compiled.
            let _ = std::fs::remove_file(&path);
        }
        Toggle::Start => {
            let parent = path.parent().context("pidfile has no parent")?;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
            let mut child = Command::new(std::env::current_exe()?)
                .args(["record", "--until-stopped"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .context("Could not start scrawl record")?;
            if let Err(error) = write_pidfile(&path, child.id()) {
                // No untracked recorder: the next press could not stop it.
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    }
    Ok(())
}

fn write_pidfile(path: &Path, pid: u32) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .context("Could not write the pidfile")?;
    write!(file, "{pid}")?;
    Ok(())
}

/// Called by the recorder once it stops listening for SIGINT: forget our pid.
pub fn clear_pidfile() {
    if let Some(path) = pidfile() {
        clear_pidfile_at(&path, std::process::id());
    }
}

/// Removes `path` only if it holds `pid`.
fn clear_pidfile_at(path: &Path, pid: u32) {
    let ours = std::fs::read_to_string(path)
        .ok()
        .and_then(|c| c.trim().parse::<u32>().ok())
        == Some(pid);
    if ours {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_live_recorder_is_stopped() {
        assert_eq!(decide(Some("4242\n"), |pid| pid == 4242), Toggle::Stop(4242));
    }

    #[test]
    fn a_dead_or_foreign_pid_starts_a_new_recording() {
        assert_eq!(decide(Some("4242"), |_| false), Toggle::Start);
    }

    #[test]
    fn no_or_garbled_pidfile_starts_a_new_recording() {
        assert_eq!(decide(None, |_| true), Toggle::Start);
        assert_eq!(decide(Some("abc"), |_| true), Toggle::Start);
        assert_eq!(decide(Some("0"), |_| true), Toggle::Start);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scrawl-toggle-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("record.pid")
    }

    #[test]
    fn our_own_pidfile_is_removed() {
        let path = scratch("ours");
        std::fs::write(&path, "77").unwrap();
        clear_pidfile_at(&path, 77);
        assert!(!path.exists());
    }

    #[test]
    fn another_recorders_pidfile_is_kept() {
        let path = scratch("other");
        std::fs::write(&path, "78").unwrap();
        clear_pidfile_at(&path, 77);
        assert!(path.exists());
    }

    #[test]
    fn a_missing_pidfile_is_a_no_op() {
        clear_pidfile_at(&scratch("missing"), 77);
    }
}
