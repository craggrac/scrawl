//! Gathering [`Evidence`] for smart routing: the browser tab, the process serving it, the
//! terminal's folder, the open sessions, and what the bundle says. All the I/O lives here;
//! [`smart::decide`](super::smart::decide) only reads what this returns.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};

use super::smart::{Address, Candidate, Clue, Evidence, Folder, Pointer, Source, words};
use super::{Hooks, WebUrl, parse_url, project_root, run_with_timeout};
use crate::handoff::{ApiGet, query_escape};
use crate::model::ScrawlBundle;

/// Pages of newest root sessions read (100 each), besides the exact-folder and by-ID reads.
pub const SESSION_PAGES: usize = 2;
/// OpenCode Desktop counts as showing the session viewed last if that was this recently.
const VIEWED_WITHIN_MS: u64 = 30 * 60_000;
const GHOSTTY: &str = "com.mitchellh.ghostty";

/// The outside steps of looking, swappable in tests.
#[derive(Clone, Copy)]
pub struct Senses {
    pub hooks: Hooks,
    /// The port on this Mac that serves `url`, when its host is this Mac.
    pub own_port: fn(&WebUrl) -> Option<u16>,
    /// The folder and title of a terminal app's focused pane.
    pub terminal: fn(&str) -> Option<(PathBuf, String)>,
}

impl Default for Senses {
    fn default() -> Self {
        Self {
            hooks: Hooks::default(),
            own_port,
            terminal: terminal_pane,
        }
    }
}

/// What can be seen when a capture finishes, before its bundle exists.
#[derive(Clone, Debug, Default)]
pub struct Live {
    pub url: Option<String>,
    pub clues: Vec<Clue>,
    pub candidates: Vec<Candidate>,
    /// Why the sessions couldn't be read, when they couldn't.
    pub error: Option<String>,
}

/// Looks at the browser tab or terminal in front (`front`) or captured (`target`), and reads
/// the open sessions. Runs while the bundle is built.
#[cfg(target_os = "macos")]
pub fn look_live(front: Option<&str>, target: Option<&str>, senses: &Senses) -> Live {
    let url = super::capture_url(front, target);
    let mut clues = url
        .as_deref()
        .map(|url| url_clues(url, senses, true))
        .unwrap_or_default();
    if let Some((cwd, title)) = [front, target]
        .into_iter()
        .flatten()
        .find_map(|id| (senses.terminal)(id))
    {
        clues.push(Clue {
            source: Source::Terminal,
            pointer: Pointer::Folder(folder_of(&cwd)),
            note: format!("the terminal's focused pane is in {}", cwd.display()),
        });
        let title_words = words(&title);
        if !title_words.is_empty() {
            clues.push(Clue {
                source: Source::Title,
                pointer: Pointer::Words { words: title_words },
                note: format!("terminal title \"{title}\""),
            });
        }
    }
    let (candidates, error) = read_for(&clues, None, senses);
    Live {
        url,
        clues,
        candidates,
        error,
    }
}

/// Reads the sessions the clues and address could name.
fn read_for(
    clues: &[Clue],
    address: Option<&Address>,
    senses: &Senses,
) -> (Vec<Candidate>, Option<String>) {
    let mut roots = Vec::new();
    let mut ids = Vec::new();
    for clue in clues {
        match &clue.pointer {
            Pointer::Folder(folder) => roots.push(folder.root.clone()),
            Pointer::Session { id, .. } => ids.push(id.clone()),
            Pointer::Words { .. } => {}
        }
    }
    ids.extend(address.map(|a| a.session().to_owned()));
    let mut api = senses.hooks.api;
    match read_candidates(&mut api, &roots, &ids, SESSION_PAGES) {
        Ok(candidates) => (candidates, None),
        Err(err) => (Vec::new(), Some(format!("couldn't list sessions: {err:#}"))),
    }
}

/// The clues a tab gives: its `/sdk.js` registration, and the folder of the process serving
/// its port when the tab is on this Mac (a loopback host, or with `own_host` this Mac's own
/// name).
pub fn url_clues(url: &str, senses: &Senses, own_host: bool) -> Vec<Clue> {
    let mut clues = Vec::new();
    if let Some(reg) = (senses.hooks.sdk)(url) {
        if let Some(id) = reg.session.clone() {
            clues.push(Clue {
                source: Source::Page,
                pointer: Pointer::Session {
                    id,
                    directory: reg.directory.as_deref().map(PathBuf::from),
                },
                note: format!("{} registered with data-session", reg.origin),
            });
        } else if let Some(dir) = reg.directory.as_deref() {
            clues.push(Clue {
                source: Source::Page,
                pointer: Pointer::Folder(folder_of(Path::new(dir))),
                note: format!("{} registered with data-directory", reg.origin),
            });
        }
    }
    let Some(parsed) = parse_url(url) else {
        return clues;
    };
    let port = if parsed.is_loopback() {
        Some(parsed.port)
    } else if own_host {
        (senses.own_port)(&parsed)
    } else {
        None
    };
    if let Some(port) = port
        && let Some(dir) = (senses.hooks.listener_dir)(port)
    {
        let via = if port == parsed.port || parsed.is_loopback() {
            format!("port {port}")
        } else {
            format!("{} (port {port} here)", parsed.origin())
        };
        clues.push(Clue {
            source: Source::Port,
            pointer: Pointer::Folder(folder_of(&dir)),
            note: format!("{via} is served from {}", dir.display()),
        });
    }
    clues
}

/// The rest of the evidence, from the finished bundle: its title, what was said, the session
/// OpenCode Desktop showed, and the destination the recording carries.
pub fn evidence_for(bundle: &ScrawlBundle, dir: &Path, live: Live) -> Evidence {
    let mut clues = live.clues;
    clues.extend(bundle_clues(bundle, &live.candidates));
    Evidence {
        address: crate::handoff::recorded_destination(dir).map(Address::Recorded),
        clues,
        candidates: live.candidates,
        now_ms: bundle.created_at_ms,
    }
}

fn bundle_clues(bundle: &ScrawlBundle, candidates: &[Candidate]) -> Vec<Clue> {
    let mut clues = Vec::new();
    let target = &bundle.target;
    if let Some(title) = target.window_title.as_deref() {
        let app = words(target.app.as_deref().unwrap_or(""));
        let title_words: Vec<String> = words(title)
            .into_iter()
            .filter(|w| !app.contains(w))
            .collect();
        if !title_words.is_empty() {
            clues.push(Clue {
                source: Source::Title,
                pointer: Pointer::Words { words: title_words },
                note: format!("window title \"{title}\""),
            });
        }
    }
    if let Some(app) = target.app.as_deref()
        && !target.bundle_id.as_deref().is_some_and(is_tool)
    {
        let last = target
            .bundle_id
            .as_deref()
            .and_then(|id| id.rsplit('.').next());
        let app_words = words(&format!("{app} {}", last.unwrap_or("")));
        if !app_words.is_empty() {
            clues.push(Clue {
                source: Source::App,
                pointer: Pointer::Words { words: app_words },
                note: format!("captured app {app}"),
            });
        }
    }
    let said = words(&bundle.transcript.text);
    if !said.is_empty() {
        clues.push(Clue {
            source: Source::Speech,
            pointer: Pointer::Words { words: said },
            note: "said".into(),
        });
    }
    if target
        .bundle_id
        .as_deref()
        .is_some_and(|id| id.to_ascii_lowercase().starts_with("ai.opencode.desktop"))
        && let Some((viewed, ms)) = last_viewed(candidates, bundle.created_at_ms)
    {
        clues.push(Clue {
            source: Source::Viewed,
            pointer: Pointer::Session {
                id: viewed.id.clone(),
                directory: None,
            },
            note: format!(
                "OpenCode Desktop showed {} last ({} before the capture ended)",
                viewed.label(),
                super::smart::ago_text(bundle.created_at_ms.saturating_sub(ms))
            ),
        });
    }
    clues
}

/// Browsers, terminals, OpenCode and the system's own apps: their names say nothing about the
/// project.
fn is_tool(bundle_id: &str) -> bool {
    const PREFIXES: [&str; 8] = [
        "com.apple.",
        "ai.opencode.",
        "com.mitchellh.ghostty",
        "com.googlecode.iterm2",
        "dev.zed.",
        "com.microsoft.vscode",
        "com.todesktop.",
        "com.tinyspeck.",
    ];
    let id = bundle_id.to_ascii_lowercase();
    super::browser_kind(&id).is_some() || PREFIXES.iter().any(|p| id.starts_with(p))
}

/// The session viewed most recently before `end`, if within [`VIEWED_WITHIN_MS`].
fn last_viewed(candidates: &[Candidate], end: u64) -> Option<(&Candidate, u64)> {
    candidates
        .iter()
        .filter_map(|c| Some((c, c.viewed_ms?)))
        .filter(|(_, ms)| *ms <= end + 5_000 && end.saturating_sub(*ms) <= VIEWED_WITHIN_MS)
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.id.cmp(&a.0.id)))
}

/// Evidence for an existing bundle: its saved `routing.json` when it has one (so the decision
/// replays exactly), else what can be gathered now, with `url` standing in for the tab.
pub fn evidence_for_bundle(
    dir: &Path,
    bundle: &ScrawlBundle,
    url: Option<&str>,
    senses: &Senses,
) -> (Evidence, Option<String>, bool) {
    if url.is_none()
        && let Some(saved) = super::read_record(dir)
    {
        return (saved, None, true);
    }
    let url = url
        .map(str::to_owned)
        .or_else(|| bundle.destination.as_ref().and_then(|d| d.url.clone()));
    let clues = url
        .as_deref()
        .map(|url| url_clues(url, senses, true))
        .unwrap_or_default();
    let address = crate::handoff::recorded_destination(dir).map(Address::Recorded);
    let (candidates, error) = read_for(&clues, address.as_ref(), senses);
    let live = Live {
        url,
        clues,
        candidates,
        error: error.clone(),
    };
    (evidence_for(bundle, dir, live), error, false)
}

// MARK: Folders

/// A folder with its project root and repository.
pub fn folder_of(path: &Path) -> Folder {
    let (root, tree) = project_root(path);
    Folder {
        path: path.to_path_buf(),
        repo: repo_of(&root),
        root,
        tree,
    }
}

/// The main checkout of the repository `dir` is in: the folder holding `.git`, or for a
/// worktree (a `.git` file) the checkout its `commondir` belongs to.
pub fn repo_of(dir: &Path) -> Option<PathBuf> {
    let mut current = Some(dir);
    while let Some(path) = current {
        let git = path.join(".git");
        if git.is_dir() {
            return Some(path.to_path_buf());
        }
        if git.is_file() {
            let text = std::fs::read_to_string(&git).ok()?;
            let gitdir = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
            let gitdir = if gitdir.is_absolute() {
                gitdir
            } else {
                path.join(gitdir)
            };
            let common = std::fs::read_to_string(gitdir.join("commondir"))
                .map(|c| gitdir.join(c.trim()))
                .unwrap_or(gitdir);
            let common = common.canonicalize().unwrap_or(common);
            return common.parent().map(Path::to_path_buf);
        }
        current = path.parent();
    }
    None
}

// MARK: Sessions

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSession {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default, rename = "parentID")]
    parent_id: Option<String>,
    #[serde(default)]
    time: RawTime,
    #[serde(default)]
    location: Option<RawLocation>,
    #[serde(default)]
    directory: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct RawTime {
    #[serde(default)]
    updated: u64,
    #[serde(default)]
    viewed: Option<u64>,
    #[serde(default)]
    archived: Option<u64>,
}

#[derive(serde::Deserialize)]
struct RawLocation {
    #[serde(default)]
    directory: Option<String>,
}

impl RawSession {
    fn open_root(&self) -> bool {
        self.parent_id.is_none() && self.time.archived.is_none()
    }

    fn candidate(self, repos: &mut HashMap<PathBuf, Option<PathBuf>>) -> Candidate {
        let directory = self
            .location
            .and_then(|l| l.directory)
            .or(self.directory)
            .filter(|d| !d.is_empty())
            .map(PathBuf::from);
        let repo = directory
            .as_ref()
            .and_then(|d| repos.entry(d.clone()).or_insert_with(|| repo_of(d)).clone());
        Candidate {
            id: self.id,
            title: self.title,
            directory,
            repo,
            updated_ms: self.time.updated,
            viewed_ms: self.time.viewed,
        }
    }
}

fn session_list(body: &[u8]) -> Result<(Vec<RawSession>, Option<String>)> {
    let response: serde_json::Value =
        serde_json::from_slice(body).context("Invalid OpenCode session response")?;
    let data = response
        .as_array()
        .or_else(|| response.get("data").and_then(|d| d.as_array()))
        .context("Missing OpenCode session list")?;
    let sessions = data
        .iter()
        .filter_map(|s| serde_json::from_value::<RawSession>(s.clone()).ok())
        .collect();
    let next = response
        .pointer("/cursor/next")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    Ok((sessions, next))
}

/// Open root sessions: the newest `pages` pages, the newest page in each of `roots`, and each
/// of `ids` (whatever its state, since something named it).
pub fn read_candidates(
    get: &mut ApiGet,
    roots: &[PathBuf],
    ids: &[String],
    pages: usize,
) -> Result<Vec<Candidate>> {
    let mut raw: Vec<RawSession> = Vec::new();
    let add = |raw: &mut Vec<RawSession>, session: RawSession| {
        if !raw.iter().any(|s| s.id == session.id) {
            raw.push(session);
        }
    };
    let mut path = "/api/session?parentID=null&limit=100".to_owned();
    let mut cursors = std::collections::HashSet::new();
    for _ in 0..pages {
        let (sessions, next) = session_list(&get(&path)?)?;
        let empty = sessions.is_empty();
        for session in sessions.into_iter().filter(RawSession::open_root) {
            add(&mut raw, session);
        }
        match next {
            Some(next) if !empty && cursors.insert(next.clone()) => {
                path = format!("/api/session?cursor={}&limit=100", query_escape(&next));
            }
            _ => break,
        }
    }
    for root in roots {
        if super::smart::is_home_or_root(root) {
            continue;
        }
        let path = format!(
            "/api/session?parentID=null&limit=100&directory={}",
            query_escape(&root.to_string_lossy())
        );
        let (sessions, _) = session_list(&get(&path)?)?;
        for session in sessions.into_iter().filter(RawSession::open_root) {
            add(&mut raw, session);
        }
    }
    for id in ids {
        if raw.iter().any(|s| &s.id == id) {
            continue;
        }
        let Ok(body) = get(&format!("/api/session/{}", query_escape(id))) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
            continue;
        };
        if let Ok(session) =
            serde_json::from_value::<RawSession>(value.get("data").unwrap_or(&value).clone())
        {
            add(&mut raw, session);
        }
    }
    let mut repos = HashMap::new();
    Ok(raw.into_iter().map(|s| s.candidate(&mut repos)).collect())
}

// MARK: This Mac's own names

/// The port on this Mac serving `url`: its own port on a loopback host, else, when the host is
/// one of this Mac's names, the loopback port `tailscale serve` proxies it to, or the same
/// port for plain `http`.
pub fn own_port(url: &WebUrl) -> Option<u16> {
    if url.is_loopback() {
        return Some(url.port);
    }
    if !own_names().iter().any(|name| *name == url.host) {
        return None;
    }
    tailscale(&["serve", "status", "--json"])
        .and_then(|json| serve_target(&json, &url.host, url.port))
        .or_else(|| (url.scheme == "http").then_some(url.port))
}

/// The loopback port `tailscale serve status --json` proxies `host:port`'s root to.
pub fn serve_target(status: &serde_json::Value, host: &str, port: u16) -> Option<u16> {
    let handlers = status
        .get("Web")?
        .as_object()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(&format!("{host}:{port}")))?
        .1
        .get("Handlers")?
        .as_object()?;
    let proxy = handlers
        .get("/")
        .or_else(|| handlers.values().next())?
        .get("Proxy")?
        .as_str()?;
    let target = parse_url(proxy).or_else(|| parse_url(&format!("http://{proxy}")))?;
    target.is_loopback().then_some(target.port)
}

/// This Mac's host names, lowercased: `hostname`, its Bonjour name, and its Tailscale name.
fn own_names() -> &'static [String] {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names = Vec::new();
        let mut push = |name: &str| {
            let name = name.trim().trim_end_matches('.').to_ascii_lowercase();
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
        };
        if let Some(out) = run(&mut Command::new("/bin/hostname")) {
            push(&out);
        }
        if let Some(out) = run(Command::new("/usr/sbin/scutil").args(["--get", "LocalHostName"])) {
            push(&format!("{}.local", out.trim()));
        }
        if let Some(status) = tailscale(&["status", "--json"])
            && let Some(dns) = status.pointer("/Self/DNSName").and_then(|v| v.as_str())
        {
            push(dns);
        }
        names
    })
}

fn run(command: &mut Command) -> Option<String> {
    let output = run_with_timeout(command, Duration::from_secs(2))?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn tailscale(args: &[&str]) -> Option<serde_json::Value> {
    const APP: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";
    let binary = if Path::new(APP).exists() {
        APP
    } else {
        "tailscale"
    };
    serde_json::from_str(&run(Command::new(binary).args(args))?).ok()
}

// MARK: Terminals

/// Ghostty's focused pane: its working directory and title (AppleScript, Ghostty 1.3+). Other
/// terminals give nothing yet.
pub fn terminal_pane(bundle_id: &str) -> Option<(PathBuf, String)> {
    if !bundle_id.eq_ignore_ascii_case(GHOSTTY) {
        return None;
    }
    let script = format!(
        "tell application id \"{GHOSTTY}\"\nif (count of windows) is 0 then return \"\"\nset t to focused terminal of selected tab of front window\nreturn (working directory of t) & linefeed & (name of t)\nend tell"
    );
    let output = run(Command::new("/usr/bin/osascript").args(["-e", &script]))?;
    let (dir, title) = output
        .trim_end()
        .split_once('\n')
        .unwrap_or((output.trim(), ""));
    let dir = PathBuf::from(dir.trim());
    (dir.is_absolute() && dir != Path::new("/")).then(|| (dir, title.trim().to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff::fake_api::session;
    use crate::web::Registration;

    #[test]
    fn tailscale_serve_maps_this_macs_name_to_a_loopback_port() {
        let status = serde_json::json!({
            "TCP": {"4948": {"HTTPS": true}},
            "Web": {
                "studio.example.ts.net:4948": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:4948"}}},
                "studio.example.ts.net:8443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:3000"}}},
                "studio.example.ts.net:9000": {"Handlers": {"/": {"Proxy": "https://example.com"}}}
            }
        });
        assert_eq!(
            serve_target(&status, "studio.example.ts.net", 4948),
            Some(4948)
        );
        assert_eq!(
            serve_target(&status, "studio.example.ts.net", 8443),
            Some(3000)
        );
        assert_eq!(serve_target(&status, "studio.example.ts.net", 9000), None);
        assert_eq!(serve_target(&status, "studio.example.ts.net", 1), None);
        assert_eq!(serve_target(&serde_json::json!({}), "x", 1), None);
    }

    #[test]
    fn worktrees_share_their_repository() {
        let base = std::env::temp_dir().join(format!("scrawl-repo-{}", crate::make_bundle_id()));
        let main = base.join("main");
        std::fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
        std::fs::write(main.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
        let wt = base.join("wt");
        std::fs::create_dir_all(wt.join("site")).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()),
        )
        .unwrap();
        let main = main.canonicalize().unwrap();
        assert_eq!(repo_of(&wt.join("site")), Some(main.clone()));
        assert_eq!(repo_of(&main), Some(main.clone()));
        let folder = folder_of(&wt.join("site"));
        assert_eq!((folder.root, folder.tree), (wt.clone(), true));
        std::fs::remove_dir_all(base).unwrap();
    }

    fn no_sdk(_: &str) -> Option<Registration> {
        None
    }
    fn listener(port: u16) -> Option<PathBuf> {
        (port == 4948).then(|| PathBuf::from("/p/board"))
    }
    fn tailnet(url: &WebUrl) -> Option<u16> {
        (url.host == "studio.example.ts.net").then_some(url.port)
    }
    fn no_terminal(_: &str) -> Option<(PathBuf, String)> {
        None
    }
    fn api(path: &str) -> Result<Vec<u8>> {
        crate::handoff::fake_api::serve(
            &[
                session("ses_board", "BOARD", "/p/board", 10),
                session("ses_other", "OTHER", "/q", 20),
            ],
            path,
        )
    }

    /// The world one ladder case sees, read by the plain-`fn` hooks below.
    #[derive(Clone, Default)]
    struct World {
        reg: Option<Registration>,
        listeners: Vec<(u16, PathBuf)>,
        sessions: Vec<crate::handoff::OpenCodeSession>,
    }

    thread_local! {
        static WORLD: std::cell::RefCell<World> = std::cell::RefCell::default();
    }

    fn world_sdk(url: &str) -> Option<Registration> {
        WORLD.with(|w| {
            let reg = w.borrow().reg.clone()?;
            (parse_url(url)?.origin() == reg.origin).then_some(reg)
        })
    }
    fn world_listener(port: u16) -> Option<PathBuf> {
        WORLD.with(|w| {
            w.borrow()
                .listeners
                .iter()
                .find(|(p, _)| *p == port)
                .map(|(_, d)| d.clone())
        })
    }
    fn world_api(path: &str) -> Result<Vec<u8>> {
        let sessions = WORLD.with(|w| w.borrow().sessions.clone());
        crate::handoff::fake_api::serve(&sessions, path)
    }
    fn world_own_port(url: &WebUrl) -> Option<u16> {
        Some(url.port)
    }

    /// Flag off is today: `Policy::Ladder` over the clues `url_clues` gathers (without this
    /// Mac's own names) decides exactly what `routing::route` does, case by case.
    #[test]
    fn the_ladder_policy_reproduces_route() {
        use super::super::smart::{Policy, decide, destination};
        let base = std::env::temp_dir().join(format!("scrawl-ladder-{}", crate::make_bundle_id()));
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("site")).unwrap();
        let repo_s = repo.to_string_lossy().into_owned();
        let site_s = repo.join("site").to_string_lossy().into_owned();
        let home = dirs::home_dir().unwrap().to_string_lossy().into_owned();

        let reg = |origin: &str, session: Option<&str>, directory: Option<&str>| Registration {
            origin: origin.into(),
            url: format!("{origin}/"),
            session: session.map(Into::into),
            directory: directory.map(Into::into),
            ..Registration::default()
        };
        let sessions = vec![
            session("ses_sdk", "SDK", "/elsewhere", 5),
            session("ses_app", "APP", "/p/app", 10),
            session("ses_app_old", "APP OLD", "/p/app", 3),
            session("ses_repo", "REPO", &repo_s, 20),
            session("ses_site", "SITE", &site_s, 30),
            session("ses_home", "HOME", &home, 40),
            session("ses_untitled", "", "/p/untitled", 50),
        ];
        let cases: Vec<(&str, World)> = vec![
            (
                "http://localhost:5173/page",
                World {
                    reg: Some(reg("http://localhost:5173", Some("ses_sdk"), None)),
                    ..World::default()
                },
            ),
            (
                "http://localhost:5173/page",
                World {
                    reg: Some(reg(
                        "http://localhost:5173",
                        Some("ses_sdk"),
                        Some("/p/app"),
                    )),
                    listeners: vec![(5173, "/p/app".into())],
                    ..World::default()
                },
            ),
            (
                "http://localhost:5173/",
                World {
                    reg: Some(reg("http://localhost:5173", Some("ses_missing"), None)),
                    ..World::default()
                },
            ),
            (
                "https://studio.ts.net:5173/",
                World {
                    reg: Some(reg("https://studio.ts.net:5173", None, Some("/p/app"))),
                    ..World::default()
                },
            ),
            (
                "http://localhost:4321/",
                World {
                    reg: Some(reg("http://localhost:4321", None, Some("/p/none"))),
                    listeners: vec![(4321, "/p/app".into())],
                    ..World::default()
                },
            ),
            (
                "http://localhost:4321/",
                World {
                    reg: Some(reg("http://localhost:4321", None, None)),
                    listeners: vec![(4321, "/p/app".into())],
                    ..World::default()
                },
            ),
            (
                "http://127.0.0.1:4321/",
                World {
                    listeners: vec![(4321, "/p/app".into())],
                    ..World::default()
                },
            ),
            (
                "http://[::1]:4000/x",
                World {
                    listeners: vec![(4000, repo.join("site/src"))],
                    ..World::default()
                },
            ),
            (
                "http://localhost:4001/",
                World {
                    listeners: vec![(4001, repo.clone())],
                    ..World::default()
                },
            ),
            (
                "http://localhost:4002/",
                World {
                    listeners: vec![(4002, home.clone().into())],
                    ..World::default()
                },
            ),
            (
                "http://localhost:4003/",
                World {
                    listeners: vec![(4003, "/p/untitled".into())],
                    ..World::default()
                },
            ),
            ("http://localhost:9999/", World::default()),
            (
                "https://studio.ts.net:4321/",
                World {
                    listeners: vec![(4321, "/p/app".into())],
                    ..World::default()
                },
            ),
            ("https://example.com/", World::default()),
            ("chrome://newtab", World::default()),
        ];
        let senses = Senses {
            hooks: Hooks {
                sdk: world_sdk,
                listener_dir: world_listener,
                api: world_api,
            },
            own_port: world_own_port,
            terminal: no_terminal,
        };
        let mut sent = 0;
        for (url, mut world) in cases {
            world.sessions = sessions.clone();
            WORLD.with(|w| *w.borrow_mut() = world);
            let today = super::super::route(url, &senses.hooks).ok();
            let clues = url_clues(url, &senses, false);
            let (candidates, _) = read_for(&clues, None, &senses);
            let evidence = Evidence {
                address: None,
                clues,
                candidates,
                now_ms: 0,
            };
            let ladder = destination(&decide(&evidence, Policy::Ladder), Some(url));
            assert_eq!(ladder, today, "{url}");
            sent += usize::from(today.is_some());
        }
        assert_eq!(sent, 9);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_tab_on_this_macs_tailnet_name_is_a_local_dev_site() {
        let senses = Senses {
            hooks: Hooks {
                sdk: no_sdk,
                listener_dir: listener,
                api,
            },
            own_port: tailnet,
            terminal: no_terminal,
        };
        let url = "https://studio.example.ts.net:4948/board";
        assert!(url_clues(url, &senses, false).is_empty());
        let clues = url_clues(url, &senses, true);
        assert_eq!(clues.len(), 1);
        assert_eq!(clues[0].source, Source::Port);
        let (candidates, error) = read_for(&clues, None, &senses);
        assert_eq!(error, None);
        let evidence = Evidence {
            address: None,
            clues,
            candidates,
            now_ms: 15,
        };
        let j = super::super::smart::decide(&evidence, super::super::smart::Policy::Weigh);
        let dest = super::super::smart::destination(&j, Some(url)).unwrap();
        assert_eq!(
            (dest.session.as_str(), dest.via.as_str()),
            ("ses_board", "localhost")
        );
        assert_eq!(dest.directory.as_deref(), Some("/p/board"));
    }
}
