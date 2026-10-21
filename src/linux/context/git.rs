//! The state of the repository a folder is in, for `Context:`: branch, head commit and what is
//! changed. The folder came from a terminal or a server, so its repository is not trusted: its
//! configuration must not start a program. `git` is run with every setting that names one
//! overridden (`fsmonitor`, hooks, pager, external diff, gpg), the system and attribute files
//! ignored, and `status` (which reads the working tree and so runs `clean` filters a repository's
//! own config can name) only when that config names no filter at all. It also runs read-only
//! (`--no-optional-locks`), at low priority, under a timeout and an output cap.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::dev_context::Project;

use super::redact;

/// Each `git` command's time before it is killed.
const GIT_TIMEOUT: Duration = Duration::from_secs(2);
/// More output than this is dropped (a repository full of changes).
const MAX_OUTPUT: u64 = 256 * 1024;
/// Changed files listed; the count is the whole.
const MAX_FILES: usize = 12;
const MAX_PATH_CHARS: usize = 120;
const MAX_SUBJECT_CHARS: usize = 80;

/// Settings that make git run a program, switched off for every command.
const NO_PROGRAMS: [&str; 11] = [
    "core.fsmonitor=false",
    "core.hooksPath=/dev/null",
    "core.pager=cat",
    "core.untrackedCache=false",
    "core.attributesFile=/dev/null",
    "diff.external=",
    "log.showSignature=false",
    "gpg.program=false",
    "gpg.ssh.program=false",
    "core.sshCommand=false",
    "core.askPass=false",
];

/// Runs `git args` in `root` at low CPU and I/O priority; its exit code (`None` when it
/// couldn't be run or ran out of time) and output.
fn git_raw(root: &Path, args: &[&str]) -> Option<(i32, String)> {
    let mut command = Command::new("git");
    command.arg("--no-optional-locks");
    for setting in NO_PROGRAMS {
        command.args(["-c", setting]);
    }
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_PAGER", "cat")
        .env_remove("GIT_EXTERNAL_DIFF")
        .env_remove("GIT_SSH_COMMAND")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: only async-signal-safe syscalls between fork and exec.
    unsafe {
        command.pre_exec(|| {
            libc::setpriority(libc::PRIO_PROCESS, 0, 19);
            // Best-effort class, lowest priority (IOPRIO_WHO_PROCESS = 1; 2 << 13 | 7).
            libc::syscall(
                libc::SYS_ioprio_set,
                1 as libc::c_long,
                0 as libc::c_long,
                ((2 << 13) | 7) as libc::c_long,
            );
            Ok(())
        });
    }
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stdout).take(MAX_OUTPUT).read_to_end(&mut bytes);
        let _ = sender.send(bytes);
    });
    match receiver.recv_timeout(GIT_TIMEOUT) {
        Ok(bytes) if bytes.len() as u64 >= MAX_OUTPUT => {
            // The rest is dropped, and git with it.
            let _ = child.kill();
            let _ = child.wait();
            Some((0, String::from_utf8_lossy(&bytes).into_owned()))
        }
        Ok(bytes) => {
            let code = child.wait().ok()?.code()?;
            Some((code, String::from_utf8_lossy(&bytes).into_owned()))
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

/// The output of `git args` when it succeeded in time.
fn git(root: &Path, args: &[&str]) -> Option<String> {
    git_raw(root, args).filter(|(code, _)| *code == 0).map(|(_, out)| out)
}

/// Whether the repository's own configuration (`.git/config`, what it includes, a worktree's)
/// names no `filter.*` program, so reading the working tree can't start one. The user's global
/// configuration is trusted and not asked about: `git-lfs` lives there.
fn config_names_no_filter(root: &Path) -> bool {
    match git_raw(root, &["config", "--show-scope", "--get-regexp", r"^filter\."]) {
        // Exit 1: nothing matched.
        Some((1, _)) => true,
        Some((0, out)) => out.lines().all(|line| {
            !matches!(line.split('\t').next(), Some("local" | "worktree" | "command"))
        }),
        _ => false,
    }
}

/// The changed entries of `git status --porcelain=v1`, each `"M src/a.rs"` (the status, a space,
/// the path).
pub fn parse_changed(output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| line.len() > 3)
        .map(|line| {
            let (status, path) = line.split_at(3);
            format!("{} {}", status.trim(), redact::text(path.trim(), MAX_PATH_CHARS))
        })
        .collect()
}

/// The repository around `dir` (`root` is its git root, found by the caller) as a project seen
/// at `t`. Fields stay empty when `git` doesn't answer, and the changes when it isn't safe to
/// ask for them.
pub fn project(t: u64, dir: &Path, root: &Path, source: &str) -> Project {
    let branch = git(root, &["symbolic-ref", "-q", "--short", "HEAD"])
        .map(|out| out.trim().to_owned())
        .filter(|name| !name.is_empty());
    let latest = git(root, &["log", "-1", "--no-show-signature", "--format=%h%x09%s"]);
    let (head, subject) = match latest.as_deref().and_then(|line| line.trim_end().split_once('\t')) {
        Some((head, subject)) => (
            Some(head.to_owned()),
            Some(redact::text(subject, MAX_SUBJECT_CHARS)),
        ),
        None => (None, None),
    };
    let changed = config_names_no_filter(root)
        .then(|| git(root, &["status", "--porcelain=v1", "--no-renames", "--ignore-submodules"]))
        .flatten()
        .map(|out| parse_changed(&out));
    Project {
        t,
        dir: dir.display().to_string(),
        source: source.to_owned(),
        root: Some(root.display().to_string()),
        branch,
        head,
        subject,
        changed: changed.as_ref().map(|files| files.len() as u32),
        files: changed.unwrap_or_default().into_iter().take(MAX_FILES).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_gives_the_changed_entries() {
        assert_eq!(
            parse_changed(" M src/a.rs\n?? notes.txt\nA  new.rs\n"),
            vec!["M src/a.rs", "?? notes.txt", "A new.rs"]
        );
        assert!(parse_changed("").is_empty());
    }

    /// A repository with one commit and two changes, in a temp directory.
    fn repository(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("scrawl-git-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(["-C"])
                .arg(&dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .stdout(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        run(&["init", "-q", "-b", "trunk"]);
        std::fs::write(dir.join("a.txt"), "one").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "Add a with token=abc123 inside"]);
        std::fs::write(dir.join("a.txt"), "two").unwrap();
        std::fs::write(dir.join("new.txt"), "x").unwrap();
        dir
    }

    #[test]
    fn a_repository_is_read_without_touching_it() {
        let dir = repository("read");
        let index_before = std::fs::metadata(dir.join(".git/index")).unwrap().modified().unwrap();
        let project = project(1_500, &dir, &dir, "terminal");
        assert_eq!(project.branch.as_deref(), Some("trunk"));
        assert_eq!(project.head.as_ref().map(String::len), Some(7));
        assert_eq!(project.subject.as_deref(), Some("Add a with token=REDACTED inside"));
        assert_eq!((project.t, project.changed), (1_500, Some(2)));
        assert_eq!(project.files, vec!["M a.txt", "?? new.txt"]);
        // `git status` would have refreshed the index; the optional lock is off.
        assert_eq!(
            std::fs::metadata(dir.join(".git/index")).unwrap().modified().unwrap(),
            index_before
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A repository whose own configuration names a program for every setting that can run one,
    /// each of which would leave a file in the returned directory; `a.txt` is modified so that
    /// reading the working tree has to look at its content.
    fn hostile_repository(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = repository(name);
        let marks = dir.join("marks");
        std::fs::create_dir_all(&marks).unwrap();
        let touch = |what: &str| format!("sh -c 'touch {}/{what}' --", marks.display());
        let hooks = dir.join("evil-hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        for hook in ["post-index-change", "reference-transaction", "post-checkout", "pre-auto-gc"] {
            let path = hooks.join(hook);
            std::fs::write(&path, format!("#!/bin/sh\ntouch {}/hook-{hook}\n", marks.display())).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(dir.join(".gitattributes"), "* filter=evil diff=evil\n").unwrap();
        for (key, value) in [
            ("core.fsmonitor", touch("fsmonitor")),
            ("core.hooksPath", hooks.display().to_string()),
            ("core.pager", touch("pager")),
            ("pager.log", touch("pager-log")),
            ("pager.status", touch("pager-status")),
            ("diff.external", touch("diff-external")),
            ("diff.evil.textconv", touch("textconv")),
            ("filter.evil.clean", format!("sh -c 'touch {}/filter-clean; cat' --", marks.display())),
            ("filter.evil.smudge", format!("sh -c 'touch {}/filter-smudge; cat' --", marks.display())),
            ("filter.evil.process", touch("filter-process")),
            ("gpg.program", touch("gpg")),
            ("gpg.ssh.program", touch("gpg-ssh")),
            ("log.showSignature", "true".to_owned()),
            ("core.sshCommand", touch("ssh")),
            ("core.askPass", touch("askpass")),
        ] {
            let status = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["config", key, &value])
                .status()
                .unwrap();
            assert!(status.success(), "git config {key}");
        }
        // Different size, so the stat check can't clear it.
        std::fs::write(dir.join("a.txt"), "changed content").unwrap();
        (dir, marks)
    }

    fn marks(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_repository_that_names_programs_in_its_config_gets_none_of_them_run() {
        let (dir, marks_dir) = hostile_repository("hostile");
        // The trap is real: an ordinary `git status` and `git log -p` start programs here.
        for args in [&["status", "--porcelain"][..], &["log", "-p", "-1", "--show-signature"]] {
            let _ = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let started = marks(&marks_dir);
        assert!(started.iter().any(|m| m.starts_with("filter-")), "{started:?}");
        assert!(started.iter().any(|m| m == "fsmonitor"), "{started:?}");
        for name in started {
            std::fs::remove_file(marks_dir.join(name)).unwrap();
        }
        // Scrawl's read starts none, and leaves the changes out rather than risk the filter.
        let project = project(0, &dir, &dir, "terminal");
        assert_eq!(marks(&marks_dir), Vec::<String>::new());
        assert_eq!(project.branch.as_deref(), Some("trunk"));
        assert!(project.head.is_some() && project.subject.is_some());
        assert_eq!((project.changed, project.files.len()), (None, 0));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_filter_in_the_repository_config_is_seen() {
        let dir = repository("filters");
        assert!(config_names_no_filter(&dir));
        let set = |key: &str| {
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["config", key, "cat"])
                .status()
                .unwrap()
        };
        set("filter.lfs.clean");
        assert!(!config_names_no_filter(&dir));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_git_that_fails_gives_no_answer() {
        assert!(git(Path::new("/nonexistent-dir"), &["status"]).is_none());
    }
}
