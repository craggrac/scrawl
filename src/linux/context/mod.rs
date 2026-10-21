//! What the capture's surroundings said, collected while it records (`devContext` in
//! `config.json`, off by default): the console errors, failed requests and page changes of the
//! browser tab in front (`cdp`, only for a browser that already exposes the DevTools Protocol),
//! the working directory of the terminal in front, and for each folder found the repository's
//! branch, head and changes (`git`). The page text is untrusted: only local pages are read
//! (`cdp::is_local_page`), and a port counts as a browser only when a process of the current
//! user listens on it (`procs`). One worker thread polls Hyprland's focused window and
//! reads the browser's events; the repositories are read on short-lived threads, so a slow
//! `git` never delays the events. The result is a `DevContext` on the capture clock.

mod cdp;
mod git;
mod procs;
mod redact;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::capture::scrawl_log;
use crate::dev_context::{DevContext, Project};

use self::cdp::{Browser, Focus, Link, Notes};
use self::procs::Proc;

/// How often the window in front is looked at.
const FOCUS_INTERVAL: Duration = Duration::from_millis(250);
/// The terminal's folder is read again this often while the same window stays in front.
const TERMINAL_INTERVAL: Duration = Duration::from_secs(2);
/// Without a browser to wait on, the loop sleeps this long between turns.
const IDLE_WAIT: Duration = Duration::from_millis(40);
/// Folders (repositories) recorded in one capture.
const MAX_PROJECTS: usize = 4;
/// Repositories still being read when the capture stops are waited for this long.
const GIT_WAIT: Duration = Duration::from_millis(1_500);
/// `stop` waits this long for the worker (a turn of its loop, `GIT_WAIT`, or the probe and
/// handshake of the port it was connecting to) and then gives up on it: finishing a
/// recording never hangs on the collector.
const STOP_WAIT: Duration = Duration::from_millis(3_500);

/// The collector of one recording. Dropping it without `stop` ends the worker.
pub struct Collector {
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<DevContext>>,
}

impl Collector {
    /// Starts collecting on the capture clock that began at `started_at`; none when the
    /// `devContext` setting is off.
    pub fn start(started_at: Instant) -> Option<Self> {
        crate::handoff::dev_context().then(|| Self::spawn(started_at, Sources::system()))?
    }

    fn spawn(started_at: Instant, sources: Sources) -> Option<Self> {
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopping);
        let worker = std::thread::Builder::new()
            .name("scrawl-dev-context".into())
            .spawn(move || run(started_at, &stop, sources))
            .inspect_err(|error| scrawl_log(format!("Dev context: no worker thread: {error}")))
            .ok()?;
        Some(Self {
            stopping,
            worker: Some(worker),
        })
    }

    /// Ends collecting and returns what it found. Takes the worker's turn at most (tens of ms),
    /// plus up to `GIT_WAIT` for a repository that is still being read; a worker that is
    /// still stuck after `STOP_WAIT` is left behind (it ends on its own, the flag is set) and
    /// nothing is returned.
    pub fn stop(self) -> DevContext {
        self.stop_within(STOP_WAIT)
    }

    fn stop_within(mut self, wait: Duration) -> DevContext {
        let began = Instant::now();
        self.stopping.store(true, Ordering::Release);
        let context = self
            .worker
            .take()
            .and_then(|worker| {
                worker.thread().unpark();
                let deadline = began + wait;
                while !worker.is_finished() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                if worker.is_finished() {
                    worker.join().ok()
                } else {
                    scrawl_log(format!("Dev context: the collector did not stop within {} ms; left behind", wait.as_millis()));
                    None
                }
            })
            .unwrap_or_default();
        scrawl_log(format!(
            "Dev context: {} page changes, {} console, {} network, {} projects; stopped in {} ms",
            context.pages.len(),
            context.console.len(),
            context.network.len(),
            context.projects.len(),
            began.elapsed().as_millis()
        ));
        context
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
    }
}

/// Where the worker looks: the focused window, the profile folders and the port from the
/// settings; swapped for fakes in the tests.
struct Sources {
    focus: Box<dyn Fn() -> Option<Focus> + Send>,
    config_dir: PathBuf,
    port: Option<u16>,
    proc: Proc,
}

impl Sources {
    fn system() -> Self {
        let own_pid = std::process::id() as i32;
        Self {
            // Scrawl's own windows (the capsule) are not what the capture is about.
            focus: Box::new(move || {
                crate::linux::hypr::active_window()
                    .filter(|window| window.pid != own_pid)
                    .map(|window| Focus {
                        pid: window.pid,
                        title: window.title,
                    })
            }),
            config_dir: dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")),
            port: crate::handoff::cdp_port(),
            proc: Proc::new(),
        }
    }
}

/// The browsers that expose DevTools, connected: each with its protocol state and socket.
/// Only a port whose listener is a process of the current user is trusted to be a browser
/// (any other local process could be listening there and answering as one); the others are
/// skipped without a word to them. Each port takes at most the probe's and the handshake's
/// deadlines, and `stopping` is looked at between ports.
fn connect_browsers(sources: &Sources, stopping: &AtomicBool) -> Vec<(Browser, Link)> {
    let mut browsers = Vec::new();
    for port in cdp::candidate_ports(&sources.config_dir, sources.port) {
        if stopping.load(Ordering::Acquire) {
            return browsers;
        }
        let Some(pid) = sources.proc.listener_pid(port) else {
            scrawl_log(format!(
                "Dev context: port {port} skipped: its listener is not a process of this user"
            ));
            continue;
        };
        let Some(path) = cdp::probe(port) else {
            continue;
        };
        if stopping.load(Ordering::Acquire) {
            return browsers;
        }
        let mut browser = Browser::new(pid);
        let opened = Link::open(port, &path).and_then(|mut link| {
            for command in browser.start() {
                link.send(command)?;
            }
            Ok(link)
        });
        match opened {
            Ok(link) => browsers.push((browser, link)),
            Err(error) => scrawl_log(format!("Dev context: browser on port {port}: {error:#}")),
        }
    }
    if browsers.is_empty() && !stopping.load(Ordering::Acquire) {
        scrawl_log(
            "Dev context: no browser exposes DevTools (start it with --remote-debugging-port=9222 \
             to collect its console and network errors)",
        );
    }
    browsers
}

/// The folders found during a capture, and the repositories read for them.
struct Projects {
    proc: Proc,
    home: Option<PathBuf>,
    /// Git roots, or folders outside a repository, already taken.
    seen: HashSet<PathBuf>,
    ports: HashSet<u16>,
    found: Arc<Mutex<Vec<Project>>>,
    /// Repository reads still running.
    reading: Arc<AtomicUsize>,
    terminal_pid: Option<i32>,
    terminal_read: Option<Instant>,
}

impl Projects {
    fn new(proc: Proc) -> Self {
        Self {
            proc,
            home: dirs::home_dir(),
            seen: HashSet::new(),
            ports: HashSet::new(),
            found: Arc::new(Mutex::new(Vec::new())),
            reading: Arc::new(AtomicUsize::new(0)),
            terminal_pid: None,
            terminal_read: None,
        }
    }

    /// The folder of the terminal in the window of process `pid`, read when it comes to the
    /// front and again every `TERMINAL_INTERVAL` while it stays.
    fn window_in_front(&mut self, t: u64, pid: i32) {
        let due = self.terminal_pid != Some(pid)
            || self.terminal_read.is_none_or(|at| at.elapsed() >= TERMINAL_INTERVAL);
        if !due {
            return;
        }
        self.terminal_pid = Some(pid);
        self.terminal_read = Some(Instant::now());
        if let Some(dir) = self.proc.terminal_cwd(pid) {
            self.add(t, dir, "terminal".to_owned());
        }
    }

    /// The folder of the process serving a `localhost` page, read once per port.
    fn page_on_port(&mut self, t: u64, port: u16) {
        if self.ports.insert(port)
            && let Some(dir) = self.proc.listener_cwd(port)
        {
            self.add(t, dir, format!("localhost:{port}"));
        }
    }

    fn add(&mut self, t: u64, dir: PathBuf, source: String) {
        // A shell at `/` or in the home folder says nothing about a project.
        if dir == Path::new("/") || self.home.as_deref() == Some(dir.as_path()) {
            return;
        }
        let (root, in_repository) = crate::routing::project_root(&dir);
        if self.seen.len() >= MAX_PROJECTS || !self.seen.insert(root.clone()) {
            return;
        }
        if !in_repository {
            self.found.lock().unwrap_or_else(PoisonError::into_inner).push(Project {
                t,
                dir: dir.display().to_string(),
                source,
                root: None,
                branch: None,
                head: None,
                subject: None,
                changed: None,
                files: Vec::new(),
            });
            return;
        }
        let (found, reading) = (Arc::clone(&self.found), Arc::clone(&self.reading));
        reading.fetch_add(1, Ordering::AcqRel);
        let spawned = std::thread::Builder::new()
            .name("scrawl-dev-context-git".into())
            .spawn({
                let reading = Arc::clone(&reading);
                move || {
                    let project = git::project(t, &dir, &root, &source);
                    found.lock().unwrap_or_else(PoisonError::into_inner).push(project);
                    reading.fetch_sub(1, Ordering::AcqRel);
                }
            });
        if spawned.is_err() {
            reading.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// The folders found, first seen first; repositories still being read are waited for up to
    /// `GIT_WAIT`.
    fn finish(self) -> Vec<Project> {
        let deadline = Instant::now() + GIT_WAIT;
        while self.reading.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut projects = std::mem::take(&mut *self.found.lock().unwrap_or_else(PoisonError::into_inner));
        projects.sort_by_key(|project| project.t);
        projects
    }
}

fn run(started_at: Instant, stopping: &AtomicBool, sources: Sources) -> DevContext {
    let elapsed_ms = || started_at.elapsed().as_millis() as u64;
    let epoch_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as f64;
    let mut notes = Notes::new(epoch_ms - elapsed_ms() as f64);
    let mut browsers = connect_browsers(&sources, stopping);
    let mut projects = Projects::new(sources.proc);
    let mut last_focus: Option<Instant> = None;
    let mut handled_pages = 0;
    while !stopping.load(Ordering::Acquire) {
        if last_focus.is_none_or(|at| at.elapsed() >= FOCUS_INTERVAL) {
            last_focus = Some(Instant::now());
            if let Some(focus) = (sources.focus)() {
                let t = elapsed_ms();
                browsers.retain_mut(|(browser, link)| {
                    browser
                        .focus(&focus, t, &mut notes)
                        .into_iter()
                        .try_for_each(|command| link.send(command))
                        .inspect_err(|error| scrawl_log(format!("Dev context: {error:#}")))
                        .is_ok()
                });
                projects.window_in_front(t, focus.pid);
            }
        }
        if browsers.is_empty() {
            std::thread::park_timeout(IDLE_WAIT);
            continue;
        }
        browsers.retain_mut(|(browser, link)| {
            let mut messages = Vec::new();
            let polled = link.poll(&mut messages);
            let t = elapsed_ms();
            let mut ok = polled.is_ok();
            for message in &messages {
                for command in browser.handle(message, t, &mut notes) {
                    ok &= link.send(command).is_ok();
                }
            }
            if let Err(error) = polled {
                scrawl_log(format!("Dev context: {error:#}"));
            }
            ok
        });
        // A `localhost` page's server: its folder is the project.
        for visit in notes.context.pages.iter().skip(handled_pages) {
            if let Some(url) = crate::routing::parse_url(&visit.url).filter(|url| url.is_loopback()) {
                projects.page_on_port(visit.t, url.port);
            }
        }
        handled_pages = notes.context.pages.len();
    }
    let mut context = notes.context;
    context.projects = projects.finish();
    context
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_page_its_errors_and_its_servers_repository_are_collected_and_stop_is_quick() {
        // This process listens, and runs in the repository the tests run from.
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let server_port = server.local_addr().unwrap().port();
        let epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as f64;
        let browser_port = cdp::testing::fake_browser(epoch_ms + 1_000.0, &format!("http://localhost:{server_port}/dash"));
        let config = std::env::temp_dir().join(format!("scrawl-ctx-{}", std::process::id()));
        std::fs::create_dir_all(config.join("browser")).unwrap();
        std::fs::write(config.join("browser/DevToolsActivePort"), format!("{browser_port}\n/devtools/browser/fake")).unwrap();
        let proc = Proc::new();
        // The browser is this process too: its window is the one in front.
        let browser_pid = proc.listener_pid(browser_port).unwrap();
        let sources = Sources {
            focus: Box::new(move || {
                Some(Focus {
                    pid: browser_pid,
                    title: "Dash - Helium".into(),
                })
            }),
            config_dir: config.clone(),
            port: None,
            proc,
        };
        let began = Instant::now();
        let collector = Collector::spawn(began, sources).unwrap();
        // The fake browser's events arrive within a few polls of the first look at the focus.
        std::thread::sleep(Duration::from_millis(1_200));
        let stopping = Instant::now();
        let context = collector.stop();
        let stop_ms = stopping.elapsed().as_millis();
        eprintln!("stop took {stop_ms} ms");
        assert!(stop_ms < 500, "stop took {stop_ms} ms");
        assert_eq!(context.pages.len(), 1);
        assert_eq!(context.console[0].text, "boom");
        assert_eq!((context.network[0].method.as_str(), context.network[0].status), ("POST", Some(500)));
        let project = &context.projects[0];
        assert_eq!(project.source, format!("localhost:{server_port}"));
        assert_eq!(
            std::path::Path::new(project.root.as_deref().unwrap()),
            std::env::current_dir().unwrap()
        );
        assert!(project.branch.is_some() && project.head.is_some());
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn with_no_browser_the_worker_still_stops_at_once() {
        let sources = Sources {
            focus: Box::new(|| None),
            config_dir: std::env::temp_dir().join("scrawl-ctx-none"),
            port: None,
            proc: Proc::new(),
        };
        let collector = Collector::spawn(Instant::now(), sources).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let began = Instant::now();
        assert!(collector.stop().is_empty());
        assert!(began.elapsed() < Duration::from_millis(300));
    }

    #[test]
    fn a_collector_stuck_in_connecting_to_slow_listeners_stops_between_ports() {
        // Three profile folders name ports whose listeners (this process) drip a byte per
        // 100 ms: each probe takes its whole deadline, and `stop` must not wait for all three.
        let config = std::env::temp_dir().join(format!("scrawl-ctx-drip-{}", std::process::id()));
        for i in 0..3 {
            let port = cdp::testing::drip_listener(Duration::from_millis(100));
            std::fs::create_dir_all(config.join(format!("b{i}"))).unwrap();
            std::fs::write(config.join(format!("b{i}/DevToolsActivePort")), format!("{port}\n/devtools/browser/x")).unwrap();
        }
        let sources = Sources {
            focus: Box::new(|| None),
            config_dir: config.clone(),
            port: None,
            proc: Proc::new(),
        };
        let collector = Collector::spawn(Instant::now(), sources).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let began = Instant::now();
        assert!(collector.stop().is_empty());
        // One probe's deadline at most (1 s), not three.
        assert!(began.elapsed() < Duration::from_millis(1_800), "{:?}", began.elapsed());
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn stop_gives_up_on_a_worker_that_does_not_return() {
        // The focus lookup hangs (a Hyprland socket that never answers, say).
        let sources = Sources {
            focus: Box::new(|| {
                std::thread::sleep(Duration::from_secs(3));
                None
            }),
            config_dir: std::env::temp_dir().join("scrawl-ctx-hang"),
            port: None,
            proc: Proc::new(),
        };
        let collector = Collector::spawn(Instant::now(), sources).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let began = Instant::now();
        assert!(collector.stop_within(Duration::from_millis(300)).is_empty());
        assert!(began.elapsed() < Duration::from_millis(1_000), "{:?}", began.elapsed());
    }

    #[test]
    fn a_port_whose_listener_is_not_this_users_is_not_connected_to() {
        // A fake browser answers on a port, but the fake /proc says another user owns it.
        let epoch_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as f64;
        let browser_port = cdp::testing::fake_browser(epoch_ms, "http://localhost:3000/");
        let config = std::env::temp_dir().join(format!("scrawl-ctx-spoof-{}", std::process::id()));
        std::fs::create_dir_all(config.join("b")).unwrap();
        std::fs::write(config.join("b/DevToolsActivePort"), format!("{browser_port}\n/devtools/browser/fake")).unwrap();
        let sources = |proc: Proc| Sources {
            focus: Box::new(|| None),
            config_dir: config.clone(),
            port: None,
            proc,
        };
        let stopping = AtomicBool::new(false);
        // Owned by root: skipped, and nothing is sent to it.
        let (root, other_user) = procs::testing::fake_listener("spoof", browser_port, 0);
        assert!(connect_browsers(&sources(other_user), &stopping).is_empty());
        let _ = std::fs::remove_dir_all(root);
        // Owned by the current user: connected, with the owning pid as the browser's window.
        let (root, same_user) = procs::testing::fake_listener("spoof-mine", browser_port, 1000);
        let browsers = connect_browsers(&sources(same_user), &stopping);
        assert_eq!(browsers.len(), 1);
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(config);
    }
}
