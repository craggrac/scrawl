//! What `/proc` says about processes: the working directory of the shell in a terminal window,
//! and of whatever listens on a TCP port (the dev server behind a `localhost` page). Read-only;
//! `Proc` takes the root so the tests can lay out a small fake one.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// The `/proc` fields about a process that find its terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub pid: i32,
    pub ppid: i32,
    /// Process group.
    pub pgrp: i32,
    /// Controlling terminal's device number; 0 for none.
    pub tty: i32,
    /// The terminal's foreground process group.
    pub tpgid: i32,
}

/// Reads `/proc/PID/stat`: `pid (comm) state ppid pgrp session tty_nr tpgid …`. `comm` may hold
/// spaces and parentheses, so the fields are counted from the last `)`.
pub fn parse_stat(text: &str) -> Option<Stat> {
    let (head, rest) = text.rsplit_once(')')?;
    let pid = head.split_once(' ')?.0.trim().parse().ok()?;
    let mut fields = rest.split_whitespace().skip(1);
    let mut next = || fields.next()?.parse::<i32>().ok();
    let ppid = next()?;
    let pgrp = next()?;
    let _session = next()?;
    let tty = next()?;
    let tpgid = next()?;
    Some(Stat {
        pid,
        ppid,
        pgrp,
        tty,
        tpgid,
    })
}

/// The inodes of TCP sockets listening on `port`, from the text of `/proc/net/tcp` or `tcp6`.
pub fn listening_inodes(table: &str, port: u16) -> Vec<u64> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // sl local_address rem_address st tx:rx tr:when retrnsmt uid timeout inode
            let (local, state, inode) = (*fields.get(1)?, *fields.get(3)?, *fields.get(9)?);
            let local_port = u16::from_str_radix(local.rsplit_once(':')?.1, 16).ok()?;
            // 0A is TCP_LISTEN.
            (state == "0A" && local_port == port)
                .then(|| inode.parse().ok())
                .flatten()
        })
        .collect()
}

pub struct Proc {
    root: PathBuf,
}

impl Proc {
    pub fn new() -> Self {
        Self::at("/proc")
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The working directory of `pid`; none when it is gone or isn't ours to read.
    pub fn cwd(&self, pid: i32) -> Option<PathBuf> {
        std::fs::read_link(self.root.join(pid.to_string()).join("cwd")).ok()
    }

    fn stats(&self) -> Vec<Stat> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
            .filter_map(|entry| parse_stat(&std::fs::read_to_string(entry.path().join("stat")).ok()?))
            .collect()
    }

    /// Where the terminal window of process `window_pid` is working: the directory of the
    /// foreground process on each terminal among its descendants. A terminal with several of
    /// them (tabs, or one server behind several windows) is only answered when they all agree;
    /// which of them is in front isn't known.
    pub fn terminal_cwd(&self, window_pid: i32) -> Option<PathBuf> {
        let stats = self.stats();
        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        for stat in &stats {
            children.entry(stat.ppid).or_default().push(stat.pid);
        }
        let by_pid: HashMap<i32, &Stat> = stats.iter().map(|s| (s.pid, s)).collect();
        let mut descendants = Vec::new();
        let mut queue = vec![window_pid];
        while let Some(pid) = queue.pop() {
            for &child in children.get(&pid).into_iter().flatten() {
                if !descendants.contains(&child) {
                    descendants.push(child);
                    queue.push(child);
                }
            }
        }
        // Per terminal: a process of the foreground group, its leader first, else the lowest pid.
        let mut foreground: BTreeMap<i32, &Stat> = BTreeMap::new();
        let rank = |s: &Stat| (s.pgrp == s.tpgid, s.pid == s.pgrp, std::cmp::Reverse(s.pid));
        for stat in descendants.iter().map(|pid| by_pid[pid]).filter(|s| s.tty != 0) {
            let slot = foreground.entry(stat.tty).or_insert(stat);
            if rank(stat) > rank(*slot) {
                *slot = stat;
            }
        }
        let mut dirs: Vec<PathBuf> = foreground
            .values()
            .filter_map(|stat| self.cwd(stat.pid))
            .collect();
        dirs.sort();
        dirs.dedup();
        match dirs.as_slice() {
            [only] => Some(only.clone()),
            _ => None,
        }
    }

    /// The working directory of the process listening on TCP `port`.
    pub fn listener_cwd(&self, port: u16) -> Option<PathBuf> {
        self.cwd(self.listener_pid(port)?)
    }

    /// The pid of the process listening on TCP `port`.
    pub fn listener_pid(&self, port: u16) -> Option<i32> {
        let mut inodes = Vec::new();
        for table in ["net/tcp", "net/tcp6"] {
            if let Ok(text) = std::fs::read_to_string(self.root.join(table)) {
                inodes.extend(listening_inodes(&text, port));
            }
        }
        if inodes.is_empty() {
            return None;
        }
        let wanted: Vec<String> = inodes.iter().map(|inode| format!("socket:[{inode}]")).collect();
        self.stats().into_iter().find_map(|stat| {
            let fds = std::fs::read_dir(self.root.join(stat.pid.to_string()).join("fd")).ok()?;
            fds.filter_map(Result::ok)
                .any(|fd| {
                    std::fs::read_link(fd.path())
                        .is_ok_and(|target| wanted.iter().any(|w| Path::new(w) == target))
                })
                .then_some(stat.pid)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_is_read_from_after_the_last_parenthesis() {
        let stat = parse_stat("4242 (my (odd) shell) S 100 4242 4200 34816 4242 4194304 1 2 3")
            .unwrap();
        assert_eq!(
            stat,
            Stat {
                pid: 4242,
                ppid: 100,
                pgrp: 4242,
                tty: 34816,
                tpgid: 4242
            }
        );
        assert!(parse_stat("garbage").is_none());
        assert!(parse_stat("1 (x) S 2").is_none());
    }

    #[test]
    fn listening_sockets_are_found_by_port_and_state() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
             0: 0100007F:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 11111 1 0 100 0 0 10 0\n\
             1: 0100007F:0BB8 0100007F:9C40 01 00000000:00000000 00:00000000 00000000  1000        0 22222 1 0 100 0 0 10 0\n\
             2: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 33333 1 0 100 0 0 10 0\n";
        assert_eq!(listening_inodes(table, 3000), vec![11111]);
        assert_eq!(listening_inodes(table, 8080), vec![33333]);
        assert!(listening_inodes(table, 9999).is_empty());
    }

    /// A fake `/proc`: `(pid, ppid, pgrp, tty, tpgid, cwd)`.
    fn fake_proc(name: &str, processes: &[(i32, i32, i32, i32, i32, &str)]) -> (PathBuf, Proc) {
        let root = std::env::temp_dir().join(format!("scrawl-proc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for &(pid, ppid, pgrp, tty, tpgid, cwd) in processes {
            let dir = root.join(pid.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("stat"),
                format!("{pid} (p) S {ppid} {pgrp} {pgrp} {tty} {tpgid} 0"),
            )
            .unwrap();
            std::os::unix::fs::symlink(cwd, dir.join("cwd")).unwrap();
        }
        (root.clone(), Proc::at(root))
    }

    #[test]
    fn a_terminals_directory_is_its_foreground_process_not_the_idle_shell() {
        // kitty (10) -> zsh (11, tty 5, foreground group is 12) -> npm (12) -> node (13).
        let (root, proc) = fake_proc(
            "fg",
            &[
                (10, 1, 10, 0, 0, "/"),
                (11, 10, 11, 5, 12, "/home/u"),
                (12, 11, 12, 5, 12, "/w/app"),
                (13, 12, 12, 5, 12, "/w/app/server"),
                (20, 1, 20, 6, 20, "/elsewhere"),
            ],
        );
        // The group leader (12), not its child, and not another terminal's shell.
        assert_eq!(proc.terminal_cwd(10), Some(PathBuf::from("/w/app")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn several_terminals_that_disagree_give_no_answer() {
        let (root, proc) = fake_proc(
            "tabs",
            &[
                (10, 1, 10, 0, 0, "/"),
                (11, 10, 11, 5, 11, "/w/a"),
                (12, 10, 12, 6, 12, "/w/b"),
            ],
        );
        assert_eq!(proc.terminal_cwd(10), None);
        let _ = std::fs::remove_dir_all(&root);
        // The same folder in both tabs is an answer.
        let (root, proc) = fake_proc(
            "same",
            &[
                (10, 1, 10, 0, 0, "/"),
                (11, 10, 11, 5, 11, "/w/a"),
                (12, 10, 12, 6, 12, "/w/a"),
            ],
        );
        assert_eq!(proc.terminal_cwd(10), Some(PathBuf::from("/w/a")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_window_without_a_terminal_has_no_directory() {
        let (root, proc) = fake_proc("none", &[(10, 1, 10, 0, 0, "/"), (11, 10, 10, 0, 0, "/x")]);
        assert_eq!(proc.terminal_cwd(10), None);
        assert_eq!(proc.terminal_cwd(999), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_listener_of_a_port_is_found_through_its_socket() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let proc = Proc::new();
        assert_eq!(proc.listener_pid(port), Some(std::process::id() as i32));
        assert_eq!(proc.listener_cwd(port), Some(std::env::current_dir().unwrap()));
        drop(listener);
        assert_eq!(proc.listener_pid(port), None);
    }
}
