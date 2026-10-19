//! `scrawl toggle`: one command for a Hyprland key bind that starts a recording, or stops the
//! one running. The running recorder's pid lives in `$XDG_RUNTIME_DIR/scrawl/record.pid`.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

/// What a key press should do.
#[derive(Debug, PartialEq, Eq)]
pub enum Toggle {
    Stop(i32),
    Start,
}

/// Where the running recorder's pid is kept.
pub fn pidfile() -> PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("scrawl/record.pid")
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
    let path = pidfile();
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
            std::fs::create_dir_all(path.parent().context("pidfile has no parent")?)?;
            let child = Command::new(std::env::current_exe()?)
                .args(["record", "--until-stopped"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .context("Could not start scrawl record")?;
            std::fs::write(&path, child.id().to_string())?;
        }
    }
    Ok(())
}

/// Called by the recorder once it stops listening for SIGINT: forget our pid.
pub fn clear_pidfile() {
    let path = pidfile();
    let ours = std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| c.trim().parse::<u32>().ok())
        == Some(std::process::id());
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
}
