//! A read-only Chrome DevTools Protocol client for the browser that already exposes one
//! (`--remote-debugging-port`, found through its `DevToolsActivePort` file or a configured
//! port): it never starts, restarts or configures a browser. One WebSocket to the browser
//! target discovers the pages (`Target`), and the tab in front (the page whose title the focused
//! window shows) is attached to with the `Runtime`, `Log` and `Network` event domains, which
//! only report: console errors and warnings, uncaught exceptions, and failed requests. The
//! page's address changes come from the target's own info. `Browser` holds the protocol state
//! and takes messages in and gives commands out, so it is tested without a browser; `Link` is
//! the socket.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Error as WsError, Message, WebSocket};

use super::redact;
use crate::dev_context::{ConsoleEntry, DevContext, NetworkFailure, PageVisit};

const PROBE_TIMEOUT: Duration = Duration::from_millis(300);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long `Link::poll` waits for the next message; the collector's loop paces on it.
pub const READ_WAIT: Duration = Duration::from_millis(40);
/// Messages read in one `poll`, so a chatty page can't keep the loop from its other work.
const MAX_MESSAGES_PER_POLL: usize = 400;
const MAX_MESSAGE_BYTES: usize = 2 << 20;

const MAX_PAGES: usize = 20;
const MAX_CONSOLE: usize = 40;
const MAX_NETWORK: usize = 40;
/// Tabs followed at once (identical titles in one window's tabs).
const MAX_ACTIVE: usize = 2;
/// Requests remembered for their method and address until they finish.
const MAX_OPEN_REQUESTS: usize = 1_024;
const TEXT_CHARS: usize = 300;
const ERROR_CHARS: usize = 60;
const SOURCE_CHARS: usize = 100;

// MARK: Finding the browser

/// The port on the first line of a `DevToolsActivePort` file.
pub fn parse_active_port(text: &str) -> Option<u16> {
    text.lines().next()?.trim().parse().ok().filter(|port| *port != 0)
}

/// The ports in `DevToolsActivePort` files one or two folders down `config` (`~/.config`):
/// Chrome and Chromium keep it in their profile folder (`google-chrome/`), Brave one level
/// deeper (`BraveSoftware/Brave-Browser/`). A browser started without a debugging port leaves
/// none; `extra` is a port named in the settings.
pub fn candidate_ports(config: &Path, extra: Option<u16>) -> Vec<u16> {
    fn subdirs(dir: &Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect()
    }
    let mut ports: Vec<u16> = Vec::new();
    for dir in subdirs(config) {
        for dir in std::iter::once(dir.clone()).chain(subdirs(&dir)) {
            if let Some(port) = std::fs::read_to_string(dir.join("DevToolsActivePort"))
                .ok()
                .and_then(|text| parse_active_port(&text))
                && !ports.contains(&port)
            {
                ports.push(port);
            }
        }
    }
    if let Some(port) = extra.filter(|port| !ports.contains(port)) {
        ports.push(port);
    }
    ports
}

/// The path of the browser target's WebSocket in a `/json/version` reply.
fn browser_socket_path(body: &str) -> Option<String> {
    let url = serde_json::from_str::<Value>(body).ok()?["webSocketDebuggerUrl"]
        .as_str()?
        .to_owned();
    let path = &url[url.find("://")? + 3..];
    let path = &path[path.find('/')?..];
    path.starts_with("/devtools/browser/").then(|| path.to_owned())
}

/// Asks `127.0.0.1:port` for `/json/version`; the browser target's WebSocket path when a
/// browser answers there (a stale `DevToolsActivePort` leaves a port nothing, or something else,
/// listens on).
pub fn probe(port: u16) -> Option<String> {
    let mut stream =
        TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), PROBE_TIMEOUT).ok()?;
    stream.set_read_timeout(Some(PROBE_TIMEOUT * 2)).ok()?;
    stream.set_write_timeout(Some(PROBE_TIMEOUT)).ok()?;
    // Chromium's DevTools server answers HTTP/1.1 only, with a Content-Length and the connection
    // kept open, so the body is read by its length.
    stream
        .write_all(format!("GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes())
        .ok()?;
    let mut reply: Vec<u8> = Vec::new();
    let mut chunk = [0_u8; 2048];
    loop {
        if let Some(end) = reply.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&reply[..end]).to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:")?.trim().parse().ok())?;
            if reply.len() >= end + 4 + length {
                return browser_socket_path(&String::from_utf8_lossy(&reply[end + 4..end + 4 + length]));
            }
        }
        let read = stream.read(&mut chunk).ok().filter(|n| *n > 0)?;
        reply.extend_from_slice(&chunk[..read]);
        if reply.len() > 64 * 1024 {
            return None;
        }
    }
}

// MARK: The socket

/// A WebSocket to the browser target.
pub struct Link {
    socket: WebSocket<TcpStream>,
}

impl Link {
    pub fn open(port: u16, path: &str) -> Result<Self> {
        let stream =
            TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), PROBE_TIMEOUT)?;
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let config = WebSocketConfig::default()
            .read_buffer_size(16 * 1024)
            .write_buffer_size(0)
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_MESSAGE_BYTES));
        let (socket, _) = tungstenite::client::client_with_config(
            format!("ws://127.0.0.1:{port}{path}"),
            stream,
            Some(config),
        )
        .map_err(|error| anyhow::anyhow!("DevTools handshake failed: {error}"))?;
        socket.get_ref().set_read_timeout(Some(READ_WAIT))?;
        Ok(Self { socket })
    }

    pub fn send(&mut self, command: String) -> Result<()> {
        self.socket
            .send(Message::text(command))
            .context("Could not send to the browser")
    }

    /// Appends the messages that arrive within `READ_WAIT` (a quiet page costs that wait, a busy
    /// one at most `MAX_MESSAGES_PER_POLL` messages); `Err` once the browser went away.
    pub fn poll(&mut self, into: &mut Vec<Value>) -> Result<()> {
        for _ in 0..MAX_MESSAGES_PER_POLL {
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    if let Ok(message) = serde_json::from_str(text.as_str()) {
                        into.push(message);
                    }
                }
                Ok(Message::Close(_)) => bail!("The browser closed the DevTools connection"),
                Ok(_) => {}
                Err(WsError::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(error) => bail!("DevTools connection: {error}"),
            }
        }
        Ok(())
    }
}

// MARK: What was seen

/// The window in front.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Focus {
    pub pid: i32,
    pub title: String,
}

/// What a recording has collected so far, with the caps and the merging of repeats.
pub struct Notes {
    pub context: DevContext,
    /// When the capture clock started, ms since the epoch: events carry their own times.
    start_epoch_ms: f64,
}

impl Notes {
    pub fn new(start_epoch_ms: f64) -> Self {
        Self {
            context: DevContext::default(),
            start_epoch_ms,
        }
    }

    fn page(&mut self, t: u64, url: &str) {
        if !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("file://"))
        {
            return;
        }
        let url = redact::url(url);
        if self.context.pages.last().is_some_and(|last| last.url == url)
            || self.context.pages.len() >= MAX_PAGES
        {
            return;
        }
        self.context.pages.push(PageVisit { t, url });
    }

    fn console(&mut self, t: u64, level: &str, text: String, at: Option<String>) {
        let entries = &mut self.context.console;
        if let Some(same) = entries
            .iter_mut()
            .find(|e| e.level == level && e.text == text && e.at == at)
        {
            same.count += 1;
        } else if entries.len() < MAX_CONSOLE {
            entries.push(ConsoleEntry {
                t,
                level: level.to_owned(),
                text,
                at,
                count: 1,
            });
        }
    }

    fn network(&mut self, t: u64, method: &str, url: &str, status: Option<u16>, error: Option<String>) {
        let url = redact::url(url);
        let entries = &mut self.context.network;
        if let Some(same) = entries
            .iter_mut()
            .find(|e| e.method == method && e.url == url && e.status == status && e.error == error)
        {
            same.count += 1;
        } else if entries.len() < MAX_NETWORK {
            entries.push(NetworkFailure {
                t,
                method: method.to_owned(),
                url,
                status,
                error,
                count: 1,
            });
        }
    }

    /// The capture time of an event stamped `timestamp_ms` (epoch), or `now_ms` without one;
    /// none when it happened before `since_ms`: what the browser replays when a domain is
    /// enabled is older than the tab's turn.
    fn event_time(&self, timestamp_ms: Option<f64>, now_ms: u64, since_ms: u64) -> Option<u64> {
        let Some(stamp) = timestamp_ms else {
            return Some(now_ms);
        };
        let t = (stamp - self.start_epoch_ms).round();
        (t >= since_ms as f64).then(|| (t as u64).min(now_ms))
    }
}

// MARK: The protocol

struct Target {
    title: String,
    url: String,
}

struct Attached {
    session: Option<String>,
    /// Events older than this belong to before the tab was followed.
    since_ms: u64,
}

/// One browser's protocol state.
pub struct Browser {
    /// The browser process: only its windows count as the focus.
    pid: Option<i32>,
    targets: HashMap<String, Target>,
    attached: HashMap<String, Attached>,
    /// `(session, request id)` → `(method, address)` of requests not finished.
    requests: HashMap<(String, String), (String, String)>,
    next_id: u64,
}

fn text_of(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_owned()
}

/// Whether a window titled `window` shows the page: Chromium titles windows `<page title>` plus
/// ` - <browser>` (and states such as ` - Audio playing`).
fn title_matches(window: &str, target: &Target) -> bool {
    let name = if target.title.is_empty() {
        target.url.split_once("://").map_or(target.url.as_str(), |(_, rest)| rest)
    } else {
        target.title.as_str()
    };
    !name.is_empty() && (window == name || window.strip_prefix(name).is_some_and(|rest| rest.starts_with(" - ")))
}

/// What a `console` argument or an exception said, on one line.
fn remote_object_text(object: &Value) -> String {
    if let Some(text) = object["value"].as_str() {
        return text.to_owned();
    }
    if !object["value"].is_null() {
        return object["value"].to_string();
    }
    // An `Error`'s description is its stack; the first line is the message.
    match object["description"].as_str() {
        Some(description) => description.lines().next().unwrap_or_default().to_owned(),
        None => object["type"].as_str().unwrap_or_default().to_owned(),
    }
}

/// `/path/file.js:12:5` for a stack frame (zero-based lines and columns in, one-based out); none
/// for code with no address and for browser extensions' own.
fn frame_text(frame: &Value) -> Option<String> {
    let url = frame["url"].as_str().filter(|url| !url.is_empty())?;
    if url.starts_with("chrome-extension://") {
        return None;
    }
    let bare = url.split(['?', '#']).next().unwrap_or(url);
    let path = bare.split_once("://").map_or(bare, |(_, rest)| rest.find('/').map_or("/", |i| &rest[i..]));
    let path: String = path.chars().take(SOURCE_CHARS).collect();
    let line = frame["lineNumber"].as_i64()? + 1;
    let column = frame["columnNumber"].as_i64().unwrap_or(0) + 1;
    Some(format!("{path}:{line}:{column}"))
}

impl Browser {
    pub fn new(pid: Option<i32>) -> Self {
        Self {
            pid,
            targets: HashMap::new(),
            attached: HashMap::new(),
            requests: HashMap::new(),
            next_id: 0,
        }
    }

    fn command(&mut self, method: &str, params: Value, session: Option<&str>) -> String {
        self.next_id += 1;
        let mut command = json!({"id": self.next_id, "method": method, "params": params});
        if let Some(session) = session {
            command["sessionId"] = json!(session);
        }
        command.to_string()
    }

    /// The first commands: report every page as it appears and changes.
    pub fn start(&mut self) -> Vec<String> {
        vec![self.command("Target.setDiscoverTargets", json!({"discover": true}), None)]
    }

    /// Follows the tab the window in front shows. A window that isn't this browser's, or one
    /// that shows none of its pages, leaves the tab followed so far (a terminal beside the
    /// browser is no reason to stop listening to the page next to it). Returns the commands to
    /// send.
    pub fn focus(&mut self, focus: &Focus, now_ms: u64, notes: &mut Notes) -> Vec<String> {
        if self.pid.is_some_and(|pid| pid != focus.pid) {
            return Vec::new();
        }
        let mut wanted: Vec<&String> = self
            .targets
            .iter()
            .filter(|(_, target)| title_matches(&focus.title, target))
            .map(|(id, _)| id)
            .collect();
        wanted.sort();
        wanted.truncate(MAX_ACTIVE);
        if wanted.is_empty() {
            return Vec::new();
        }
        let wanted: Vec<String> = wanted.into_iter().cloned().collect();
        let mut commands = Vec::new();
        let leaving: Vec<String> = self
            .attached
            .keys()
            .filter(|id| !wanted.contains(id))
            .cloned()
            .collect();
        for id in leaving {
            if let Some(Attached { session: Some(session), .. }) = self.attached.remove(&id) {
                commands.push(self.command("Target.detachFromTarget", json!({"sessionId": session}), None));
            }
        }
        // The first tab followed counts from the start of the capture; later ones from now.
        let first = notes.context.pages.is_empty() && self.attached.is_empty();
        for id in wanted {
            if self.attached.contains_key(&id) {
                continue;
            }
            let since_ms = if first { 0 } else { now_ms.saturating_sub(300) };
            self.attached.insert(id.clone(), Attached { session: None, since_ms });
            notes.page(now_ms, &self.targets[&id].url);
            commands.push(self.command(
                "Target.attachToTarget",
                json!({"targetId": id, "flatten": true}),
                None,
            ));
        }
        commands
    }

    /// Takes one message from the browser; returns the commands to send in answer.
    pub fn handle(&mut self, message: &Value, now_ms: u64, notes: &mut Notes) -> Vec<String> {
        let Some(method) = message["method"].as_str() else {
            return Vec::new();
        };
        let params = &message["params"];
        match method {
            "Target.targetCreated" | "Target.targetInfoChanged" => {
                let info = &params["targetInfo"];
                if info["type"] == "page" {
                    let id = text_of(info, "targetId");
                    let url = text_of(info, "url");
                    if self.attached.contains_key(&id)
                        && self.targets.get(&id).is_none_or(|known| known.url != url)
                    {
                        notes.page(now_ms, &url);
                    }
                    self.targets.insert(id, Target { title: text_of(info, "title"), url });
                }
            }
            "Target.targetDestroyed" => {
                let id = text_of(params, "targetId");
                self.targets.remove(&id);
                self.attached.remove(&id);
            }
            "Target.attachedToTarget" => {
                let id = text_of(&params["targetInfo"], "targetId");
                let session = text_of(params, "sessionId");
                if let Some(attached) = self.attached.get_mut(&id) {
                    attached.session = Some(session.clone());
                    return self.enable(&session);
                }
                // The tab was left before the browser answered.
                return vec![self.command("Target.detachFromTarget", json!({"sessionId": session}), None)];
            }
            "Target.detachedFromTarget" => {
                let session = text_of(params, "sessionId");
                self.attached.retain(|_, attached| attached.session.as_deref() != Some(&session));
            }
            _ => {
                if let Some(session) = message["sessionId"].as_str()
                    && let Some(since_ms) = self
                        .attached
                        .values()
                        .find(|a| a.session.as_deref() == Some(session))
                        .map(|a| a.since_ms)
                {
                    self.session_event(session, method, params, now_ms, since_ms, notes);
                }
            }
        }
        Vec::new()
    }

    /// The event domains a followed tab is listened to on: all of them only report.
    fn enable(&mut self, session: &str) -> Vec<String> {
        vec![
            self.command("Runtime.enable", json!({}), Some(session)),
            self.command("Log.enable", json!({}), Some(session)),
            // Small buffers: response bodies are never asked for, so none need keeping.
            self.command(
                "Network.enable",
                json!({"maxTotalBufferSize": 65_536, "maxResourceBufferSize": 32_768}),
                Some(session),
            ),
        ]
    }

    fn session_event(
        &mut self,
        session: &str,
        method: &str,
        params: &Value,
        now_ms: u64,
        since_ms: u64,
        notes: &mut Notes,
    ) {
        match method {
            "Runtime.consoleAPICalled" => {
                let level = match params["type"].as_str() {
                    Some("error" | "assert") => "error",
                    Some("warning") => "warning",
                    _ => return,
                };
                let frame = params["stackTrace"]["callFrames"].get(0);
                if frame.is_some_and(|f| f["url"].as_str().is_some_and(|u| u.starts_with("chrome-extension://"))) {
                    return;
                }
                let Some(t) = notes.event_time(params["timestamp"].as_f64(), now_ms, since_ms) else {
                    return;
                };
                let text = params["args"]
                    .as_array()
                    .map(|args| args.iter().map(remote_object_text).collect::<Vec<_>>().join(" "))
                    .unwrap_or_default();
                notes.console(t, level, redact::text(&text, TEXT_CHARS), frame.and_then(frame_text));
            }
            "Runtime.exceptionThrown" => {
                let details = &params["exceptionDetails"];
                let Some(t) = notes.event_time(params["timestamp"].as_f64(), now_ms, since_ms) else {
                    return;
                };
                let exception = &details["exception"];
                let mut text = remote_object_text(exception);
                if text.is_empty() {
                    text = text_of(details, "text");
                } else if exception["description"].is_null() {
                    text = format!("Uncaught {text}");
                }
                let frame = details["stackTrace"]["callFrames"].get(0).and_then(frame_text).or_else(|| {
                    frame_text(&json!({
                        "url": details["url"],
                        "lineNumber": details["lineNumber"],
                        "columnNumber": details["columnNumber"],
                    }))
                });
                notes.console(t, "exception", redact::text(&text, TEXT_CHARS), frame);
            }
            "Log.entryAdded" => {
                let entry = &params["entry"];
                // Console calls and exceptions come through `Runtime`, failed requests with
                // their method and status through `Network`.
                if entry["level"] != "error"
                    || matches!(entry["source"].as_str(), Some("network" | "console-api" | "javascript"))
                {
                    return;
                }
                let Some(t) = notes.event_time(entry["timestamp"].as_f64(), now_ms, since_ms) else {
                    return;
                };
                let at = frame_text(&json!({
                    "url": entry["url"], "lineNumber": entry["lineNumber"],
                }));
                notes.console(t, "error", redact::text(entry["text"].as_str().unwrap_or_default(), TEXT_CHARS), at);
            }
            "Network.requestWillBeSent" => {
                if self.requests.len() >= MAX_OPEN_REQUESTS {
                    self.requests.clear();
                }
                let request = &params["request"];
                self.requests.insert(
                    (session.to_owned(), text_of(params, "requestId")),
                    (text_of(request, "method"), text_of(request, "url")),
                );
            }
            "Network.responseReceived" => {
                let status = params["response"]["status"].as_u64().unwrap_or(0);
                let key = (session.to_owned(), text_of(params, "requestId"));
                let (method, mut url) = self.requests.get(&key).cloned().unwrap_or_default();
                if url.is_empty() {
                    url = text_of(&params["response"], "url");
                }
                if status >= 400 && interesting(&url) {
                    let method = if method.is_empty() { "GET" } else { &method };
                    notes.network(now_ms, method, &url, Some(status as u16), None);
                }
            }
            "Network.loadingFailed" => {
                let key = (session.to_owned(), text_of(params, "requestId"));
                let Some((method, url)) = self.requests.remove(&key) else {
                    return;
                };
                // A navigation away or a cancelled fetch isn't a failure.
                if params["canceled"] == true || !interesting(&url) {
                    return;
                }
                let reason = params["corsErrorStatus"]["corsError"]
                    .as_str()
                    .or_else(|| params["blockedReason"].as_str())
                    .unwrap_or_else(|| params["errorText"].as_str().unwrap_or("failed"));
                let error: String = reason.chars().take(ERROR_CHARS).collect();
                notes.network(now_ms, &method, &url, None, Some(error));
            }
            "Network.loadingFinished" => {
                self.requests.remove(&(session.to_owned(), text_of(params, "requestId")));
            }
            _ => {}
        }
    }
}

/// Failed requests worth a line: web requests, and not the favicon every dev server lacks.
fn interesting(url: &str) -> bool {
    (url.starts_with("http://") || url.starts_with("https://"))
        && !url.split(['?', '#']).next().unwrap_or(url).ends_with("/favicon.ico")
}

/// A fake DevTools endpoint for the tests of this module and of the collector.
#[cfg(test)]
pub mod testing {
    use super::*;

    pub fn page(id: &str, title: &str, url: &str) -> Value {
        json!({"method": "Target.targetCreated", "params": {"targetInfo":
            {"targetId": id, "type": "page", "title": title, "url": url}}})
    }

    /// A browser's DevTools endpoint for one connection: answers `/json/version`, then speaks
    /// just enough protocol to show a page, accept an attach and report an error and a 500.
    pub fn fake_browser(epoch_ms: f64, page_url: &str) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let page_url = page_url.to_owned();
        std::thread::spawn(move || {
            let (mut http, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0_u8];
                http.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            assert!(request.starts_with(b"GET /json/version"));
            let body = format!(
                r#"{{"webSocketDebuggerUrl":"ws://127.0.0.1:{port}/devtools/browser/fake"}}"#
            );
            write!(http, "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
            drop(http);
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            loop {
                let Ok(Message::Text(text)) = socket.read() else { return };
                let command: Value = serde_json::from_str(text.as_str()).unwrap();
                let mut send = |value: Value| socket.send(Message::text(value.to_string())).unwrap();
                match command["method"].as_str().unwrap() {
                    "Target.setDiscoverTargets" => send(page("P", "Dash", &page_url)),
                    "Target.attachToTarget" => send(json!({"method": "Target.attachedToTarget", "params":
                        {"sessionId": "S1", "targetInfo": {"targetId": "P"}}})),
                    "Network.enable" => {
                        assert_eq!(command["sessionId"], "S1");
                        send(json!({"method": "Runtime.consoleAPICalled", "sessionId": "S1", "params": {
                            "type": "error", "timestamp": epoch_ms + 5.0, "args": [{"type": "string", "value": "boom"}]}}));
                        send(json!({"method": "Network.requestWillBeSent", "sessionId": "S1", "params":
                            {"requestId": "r", "request": {"method": "POST", "url": "http://localhost:3000/api/x"}}}));
                        send(json!({"method": "Network.responseReceived", "sessionId": "S1", "params":
                            {"requestId": "r", "response": {"url": "http://localhost:3000/api/x", "status": 500}}}));
                    }
                    _ => {}
                }
            }
        });
        port
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{fake_browser, page};
    use super::*;
    use std::process::Stdio;

    const START_EPOCH: f64 = 1_000_000.0;

    fn notes() -> Notes {
        Notes::new(START_EPOCH)
    }

    /// A browser that follows `page-1` with session `s1`.
    fn following(notes: &mut Notes) -> Browser {
        let mut browser = Browser::new(Some(77));
        browser.handle(&page("page-1", "Dash", "http://localhost:3000/dash"), 0, notes);
        browser.handle(&page("page-2", "Mail", "https://mail.test/"), 0, notes);
        let commands = browser.focus(&Focus { pid: 77, title: "Dash - Helium".into() }, 100, notes);
        assert_eq!(commands.len(), 1);
        assert!(commands[0].contains("Target.attachToTarget") && commands[0].contains("page-1"));
        let enable = browser.handle(
            &json!({"method": "Target.attachedToTarget", "params":
                {"sessionId": "s1", "targetInfo": {"targetId": "page-1"}}}),
            110,
            notes,
        );
        let methods: Vec<String> = enable
            .iter()
            .map(|c| serde_json::from_str::<Value>(c).unwrap()["method"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(methods, ["Runtime.enable", "Log.enable", "Network.enable"]);
        assert!(enable.iter().all(|c| c.contains("\"sessionId\":\"s1\"")));
        browser
    }

    fn event(method: &str, params: Value) -> Value {
        json!({"method": method, "sessionId": "s1", "params": params})
    }

    #[test]
    fn only_the_tab_the_focused_window_shows_is_followed() {
        let mut notes = notes();
        let mut browser = following(&mut notes);
        // The first tab's address is the page at the start.
        assert_eq!(notes.context.pages, vec![PageVisit { t: 100, url: "http://localhost:3000/dash".into() }]);
        // Another application's window, another browser's: nothing changes.
        assert!(browser.focus(&Focus { pid: 5, title: "Mail - Helium".into() }, 200, &mut notes).is_empty());
        assert!(browser.focus(&Focus { pid: 77, title: "Unrelated".into() }, 200, &mut notes).is_empty());
        // Switching tabs detaches the old one and attaches the new one.
        let commands = browser.focus(&Focus { pid: 77, title: "Mail - Audio playing - Helium".into() }, 5_000, &mut notes);
        assert!(commands[0].contains("Target.detachFromTarget") && commands[0].contains("s1"));
        assert!(commands[1].contains("page-2"));
        assert_eq!(notes.context.pages.last().unwrap().url, "https://mail.test/");
    }

    #[test]
    fn a_tabs_address_changes_are_pages_and_secrets_are_cut() {
        let mut notes = notes();
        let mut browser = following(&mut notes);
        let changed = |url: &str| json!({"method": "Target.targetInfoChanged", "params": {"targetInfo":
            {"targetId": "page-1", "type": "page", "title": "Dash", "url": url}}});
        browser.handle(&changed("http://localhost:3000/settings?token=abc"), 3_000, &mut notes);
        // The same address again, and a change of a tab that isn't followed, add nothing.
        browser.handle(&changed("http://localhost:3000/settings?token=abc"), 3_100, &mut notes);
        browser.handle(&json!({"method": "Target.targetInfoChanged", "params": {"targetInfo":
            {"targetId": "page-2", "type": "page", "title": "Mail", "url": "https://mail.test/x"}}}), 3_200, &mut notes);
        let urls: Vec<&str> = notes.context.pages.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(urls, ["http://localhost:3000/dash", "http://localhost:3000/settings?token=REDACTED"]);
        assert_eq!(notes.context.pages[1].t, 3_000);
    }

    #[test]
    fn console_errors_warnings_and_exceptions_are_kept_with_their_frame() {
        let mut notes = notes();
        let mut browser = following(&mut notes);
        let stamp = START_EPOCH + 7_000.0;
        let frame = json!({"url": "http://localhost:3000/src/app.js?v=3", "lineNumber": 11, "columnNumber": 4});
        browser.handle(&event("Runtime.consoleAPICalled", json!({"type": "error", "timestamp": stamp,
            "args": [{"type": "string", "value": "Failed to load"}, {"type": "object", "description": "Error: boom\n    at f (app.js:1:1)"}],
            "stackTrace": {"callFrames": [frame]}})), 7_050, &mut notes);
        browser.handle(&event("Runtime.consoleAPICalled", json!({"type": "log", "timestamp": stamp, "args": []})), 7_050, &mut notes);
        for _ in 0..2 {
            browser.handle(&event("Runtime.consoleAPICalled", json!({"type": "warning", "timestamp": stamp + 100.0,
                "args": [{"type": "string", "value": "deprecated"}]})), 7_200, &mut notes);
        }
        browser.handle(&event("Runtime.exceptionThrown", json!({"timestamp": stamp + 500.0, "exceptionDetails": {
            "text": "Uncaught", "exception": {"type": "object", "description": "TypeError: x is not a function\n    at go (app.js:5:3)"},
            "stackTrace": {"callFrames": [frame]}}})), 7_600, &mut notes);
        browser.handle(&event("Log.entryAdded", json!({"entry": {"source": "security", "level": "error",
            "text": "Refused to load the script", "timestamp": stamp + 900.0}})), 7_900, &mut notes);
        browser.handle(&event("Log.entryAdded", json!({"entry": {"source": "network", "level": "error", "text": "Failed to load resource", "timestamp": stamp}})), 7_000, &mut notes);
        browser.handle(&event("Log.entryAdded", json!({"entry": {"source": "other", "level": "info", "text": "hello", "timestamp": stamp}})), 7_000, &mut notes);
        let seen: Vec<(u64, &str, &str, Option<&str>, u32)> = notes
            .context
            .console
            .iter()
            .map(|e| (e.t, e.level.as_str(), e.text.as_str(), e.at.as_deref(), e.count))
            .collect();
        assert_eq!(
            seen,
            [
                (7_000, "error", "Failed to load Error: boom", Some("/src/app.js:12:5"), 1),
                (7_100, "warning", "deprecated", None, 2),
                (7_500, "exception", "TypeError: x is not a function", Some("/src/app.js:12:5"), 1),
                (7_900, "error", "Refused to load the script", None, 1),
            ]
        );
    }

    #[test]
    fn a_thrown_string_and_an_extensions_noise() {
        let mut notes = notes();
        let mut browser = following(&mut notes);
        browser.handle(&event("Runtime.exceptionThrown", json!({"timestamp": START_EPOCH + 500.0, "exceptionDetails": {
            "text": "Uncaught", "exception": {"type": "string", "value": "nope"}, "url": "http://localhost:3000/a.js",
            "lineNumber": 0, "columnNumber": 9}})), 600, &mut notes);
        browser.handle(&event("Runtime.consoleAPICalled", json!({"type": "error", "timestamp": START_EPOCH + 600.0,
            "args": [{"type": "string", "value": "ext"}], "stackTrace": {"callFrames": [
                {"url": "chrome-extension://abc/content.js", "lineNumber": 1, "columnNumber": 1}]}})), 700, &mut notes);
        assert_eq!(notes.context.console.len(), 1);
        assert_eq!(notes.context.console[0].text, "Uncaught nope");
        assert_eq!(notes.context.console[0].at.as_deref(), Some("/a.js:1:10"));
    }

    #[test]
    fn what_the_browser_replays_from_before_the_capture_or_the_tabs_turn_is_dropped() {
        let mut notes = notes();
        let mut browser = following(&mut notes);
        // The first tab followed counts from the start of the capture, not before it.
        browser.handle(&event("Runtime.consoleAPICalled", json!({"type": "error", "timestamp": START_EPOCH - 5_000.0,
            "args": [{"type": "string", "value": "old"}]})), 120, &mut notes);
        assert!(notes.context.console.is_empty());
        // A tab switched to later keeps only what happened from just before its turn.
        browser.focus(&Focus { pid: 77, title: "Mail - Helium".into() }, 9_000, &mut notes);
        browser.handle(&json!({"method": "Target.attachedToTarget", "params":
            {"sessionId": "s2", "targetInfo": {"targetId": "page-2"}}}), 9_010, &mut notes);
        for (stamp, text) in [(2_000.0, "background"), (8_900.0, "just before"), (9_050.0, "now")] {
            browser.handle(&json!({"method": "Runtime.consoleAPICalled", "sessionId": "s2", "params":
                {"type": "error", "timestamp": START_EPOCH + stamp, "args": [{"type": "string", "value": text}]}}),
                9_100, &mut notes);
        }
        let texts: Vec<&str> = notes.context.console.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["just before", "now"]);
    }

    #[test]
    fn failed_requests_are_kept_with_method_and_secrets_cut() {
        let mut notes = notes();
        let mut browser = following(&mut notes);
        let request = |id: &str, method: &str, url: &str| event("Network.requestWillBeSent",
            json!({"requestId": id, "request": {"method": method, "url": url}}));
        browser.handle(&request("1", "POST", "http://localhost:3000/api/x?api_key=zzz"), 8_000, &mut notes);
        browser.handle(&event("Network.responseReceived", json!({"requestId": "1", "type": "Fetch",
            "response": {"url": "http://localhost:3000/api/x?api_key=zzz", "status": 500}})), 9_000, &mut notes);
        // Fine, redirected-away, favicon and cancelled requests leave nothing.
        browser.handle(&request("2", "GET", "http://localhost:3000/ok"), 9_000, &mut notes);
        browser.handle(&event("Network.responseReceived", json!({"requestId": "2", "response": {"url": "http://localhost:3000/ok", "status": 200}})), 9_100, &mut notes);
        browser.handle(&request("3", "GET", "http://localhost:3000/favicon.ico"), 9_100, &mut notes);
        browser.handle(&event("Network.responseReceived", json!({"requestId": "3", "response": {"url": "http://localhost:3000/favicon.ico", "status": 404}})), 9_200, &mut notes);
        browser.handle(&request("4", "GET", "http://localhost:3000/slow"), 9_200, &mut notes);
        browser.handle(&event("Network.loadingFailed", json!({"requestId": "4", "errorText": "net::ERR_ABORTED", "canceled": true})), 9_300, &mut notes);
        // A refused connection, and a CORS block.
        browser.handle(&request("5", "GET", "https://cdn.example.com/lib.js"), 9_300, &mut notes);
        browser.handle(&event("Network.loadingFailed", json!({"requestId": "5", "errorText": "net::ERR_CONNECTION_REFUSED"})), 9_400, &mut notes);
        browser.handle(&request("6", "GET", "https://api.other.test/v1"), 9_500, &mut notes);
        browser.handle(&event("Network.loadingFailed", json!({"requestId": "6", "errorText": "net::ERR_FAILED",
            "corsErrorStatus": {"corsError": "MissingAllowOriginHeader"}})), 9_600, &mut notes);
        let seen: Vec<(u64, &str, &str, Option<u16>, Option<&str>)> = notes
            .context
            .network
            .iter()
            .map(|n| (n.t, n.method.as_str(), n.url.as_str(), n.status, n.error.as_deref()))
            .collect();
        assert_eq!(
            seen,
            [
                (9_000, "POST", "http://localhost:3000/api/x?api_key=REDACTED", Some(500), None),
                (9_400, "GET", "https://cdn.example.com/lib.js", None, Some("net::ERR_CONNECTION_REFUSED")),
                (9_600, "GET", "https://api.other.test/v1", None, Some("MissingAllowOriginHeader")),
            ]
        );
    }

    #[test]
    fn counts_are_capped_and_repeats_merge() {
        let mut notes = notes();
        for i in 0..100 {
            notes.console(i, "error", format!("e{}", i % 60), None);
            notes.network(i, "GET", &format!("http://a.test/{}", i % 60), Some(500), None);
        }
        assert_eq!(notes.context.console.len(), MAX_CONSOLE);
        assert_eq!(notes.context.network.len(), MAX_NETWORK);
        // The first repeats merged into their first entry.
        assert_eq!(notes.context.console[0].count, 2);
    }

    #[test]
    fn ports_come_from_the_activity_files_of_profile_folders() {
        let root = std::env::temp_dir().join(format!("scrawl-cdp-ports-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (dir, text) in [
            ("net.imput.helium", "9222\n/devtools/browser/abc"),
            ("BraveSoftware/Brave-Browser", "40123\n/devtools/browser/def"),
            ("stale", "not a port"),
            ("other/deep/er", "1111\n/x"),
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join("DevToolsActivePort"), text).unwrap();
        }
        std::fs::create_dir_all(root.join("none")).unwrap();
        let mut ports = candidate_ports(&root, Some(9333));
        ports.sort_unstable();
        assert_eq!(ports, [9222, 9333, 40123]);
        // A configured port already found is not listed twice.
        assert_eq!(candidate_ports(&root, Some(9222)).len(), 2);
        assert!(candidate_ports(&root.join("missing"), None).is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_browser_socket_is_read_from_the_version_reply() {
        let body = r#"{"Browser":"Chrome/140","webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/browser/1b2c-3d"}"#;
        assert_eq!(browser_socket_path(body).as_deref(), Some("/devtools/browser/1b2c-3d"));
        assert_eq!(browser_socket_path("{}"), None);
        assert_eq!(browser_socket_path(r#"{"webSocketDebuggerUrl":"ws://x/other"}"#), None);
        assert_eq!(browser_socket_path("<html>"), None);
    }

    #[test]
    fn titles_match_with_the_browsers_suffix_only() {
        let target = |title: &str, url: &str| Target { title: title.into(), url: url.into() };
        let dash = target("Dash", "http://x/");
        assert!(title_matches("Dash - Helium", &dash));
        assert!(title_matches("Dash", &dash));
        assert!(!title_matches("Dashboard - Helium", &dash));
        assert!(!title_matches("Dash-ish - Helium", &dash));
        // No title: the window shows the address.
        assert!(title_matches("localhost:3000/ - Chromium", &target("", "http://localhost:3000/")));
        assert!(!title_matches("anything", &target("", "")));
    }

    #[test]
    fn a_session_with_a_browser_over_a_real_socket() {
        let epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as f64;
        let port = fake_browser(epoch_ms, "http://localhost:3000/dash");
        let path = probe(port).expect("the version reply names the browser socket");
        assert_eq!(path, "/devtools/browser/fake");
        let mut link = Link::open(port, &path).unwrap();
        let mut browser = Browser::new(None);
        let mut notes = Notes::new(epoch_ms);
        for command in browser.start() {
            link.send(command).unwrap();
        }
        let focus = Focus { pid: 1, title: "Dash - Helium".into() };
        let began = std::time::Instant::now();
        while notes.context.network.is_empty() || notes.context.console.is_empty() {
            assert!(began.elapsed() < Duration::from_secs(5), "{:?}", notes.context);
            let mut messages = Vec::new();
            link.poll(&mut messages).unwrap();
            let t = began.elapsed().as_millis() as u64;
            for command in browser.focus(&focus, t, &mut notes) {
                link.send(command).unwrap();
            }
            for message in &messages {
                for command in browser.handle(message, t, &mut notes) {
                    link.send(command).unwrap();
                }
            }
        }
        assert_eq!(notes.context.console[0].text, "boom");
        assert_eq!(
            (notes.context.network[0].method.as_str(), notes.context.network[0].status),
            ("POST", Some(500))
        );
        assert_eq!(notes.context.pages[0].url, "http://localhost:3000/dash");
    }

    #[test]
    fn a_port_nothing_listens_on_is_not_a_browser() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert_eq!(probe(port), None);
    }

    const TEST_PAGE: &str = "<!doctype html><html><head><title>Scrawl CDP Test</title></head><body><h1>hi</h1><script>\
        console.log('plain'); console.error('boom from console', new Error('inner')); console.warn('careful');\
        setTimeout(() => { fetch('/api/x?api_key=abcdefghijklmnopqrstuvwxyz123456', {method: 'POST'}); \
          fetch('/missing.json'); }, 1500);\
        setTimeout(() => undefinedFunction(), 2000);\
        setTimeout(() => history.pushState({}, '', '/route/two?token=sekret'), 2500);\
        </script></body></html>";

    /// A web server for the test page: `/api/` answers 500, the page 200, the rest 404.
    fn serve_test_page() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                std::thread::spawn(move || {
                    let mut request = [0_u8; 2048];
                    let read = stream.read(&mut request).unwrap_or(0);
                    let head = String::from_utf8_lossy(&request[..read]).into_owned();
                    let path = head.split_whitespace().nth(1).unwrap_or("/");
                    let (status, body) = match path {
                        "/" => ("200 OK", TEST_PAGE),
                        p if p.starts_with("/api/") => ("500 Internal Server Error", "oops"),
                        _ => ("404 Not Found", ""),
                    };
                    let _ = write!(
                        stream,
                        "HTTP/1.0 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        port
    }

    /// Against a real headless Chromium that exposes DevTools (`SCRAWL_TEST_CHROMIUM` names the
    /// binary, else `chromium`); nothing is shown on screen. Run with `--ignored`.
    #[test]
    #[ignore = "needs a Chromium; cargo test -- --ignored chromium"]
    fn a_real_chromium_reports_a_pages_errors() {
        struct Kill(std::process::Child);
        impl Drop for Kill {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let root = std::env::temp_dir().join(format!("scrawl-cdp-real-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let profile = root.join("chromium-test-profile");
        std::fs::create_dir_all(&profile).unwrap();
        let web = serve_test_page();
        let epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as f64;
        let began = std::time::Instant::now();
        let _browser = Kill(
            std::process::Command::new(
                std::env::var("SCRAWL_TEST_CHROMIUM").unwrap_or_else(|_| "chromium".into()),
            )
            .args(["--headless=new", "--no-sandbox", "--disable-gpu", "--remote-debugging-port=0"])
            .arg(format!("--user-data-dir={}", profile.display()))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("could not start chromium"),
        );
        let (port, path) = loop {
            assert!(began.elapsed() < Duration::from_secs(20), "no DevToolsActivePort");
            if let Some(port) = candidate_ports(&root, None).first().copied()
                && let Some(path) = probe(port)
            {
                break (port, path);
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let mut link = Link::open(port, &path).unwrap();
        let mut browser = Browser::new(None);
        let mut notes = Notes::new(epoch_ms);
        for command in browser.start() {
            link.send(command).unwrap();
        }
        // The tab is opened after the connection: a tab still loading when DevTools connects
        // keeps its address as its title until it navigates.
        link.send(json!({"id": 900, "method": "Target.createTarget",
            "params": {"url": format!("http://127.0.0.1:{web}/")}}).to_string()).unwrap();
        while began.elapsed() < Duration::from_secs(12)
            && !(notes.context.pages.len() >= 2 && notes.context.network.len() >= 2)
        {
            let mut messages = Vec::new();
            link.poll(&mut messages).unwrap();
            let t = began.elapsed().as_millis() as u64;
            // The window in front is titled as the page is, as the browser knows it.
            let shown = browser
                .targets
                .values()
                .find(|target| target.url.starts_with("http://127.0.0.1"))
                .map(|target| match target.title.as_str() {
                    "" => target.url.clone(),
                    title => title.to_owned(),
                });
            if let Some(shown) = shown {
                let focus = Focus { pid: 1, title: format!("{} - Chromium", shown.trim_start_matches("http://")) };
                for command in browser.focus(&focus, t, &mut notes) {
                    link.send(command).unwrap();
                }
            }
            for message in &messages {
                for command in browser.handle(message, t, &mut notes) {
                    link.send(command).unwrap();
                }
            }
        }
        eprintln!("{}", serde_json::to_string_pretty(&notes.context).unwrap());
        eprintln!("{}", notes.context.summary_lines(None).join("\n"));
        let context = &notes.context;
        let urls: Vec<&str> = context.pages.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(urls[0], format!("http://127.0.0.1:{web}/"));
        assert_eq!(urls[1], format!("http://127.0.0.1:{web}/route/two?token=REDACTED"));
        let texts: Vec<(&str, &str)> = context.console.iter().map(|e| (e.level.as_str(), e.text.as_str())).collect();
        assert!(texts.contains(&("error", "boom from console Error: inner")), "{texts:?}");
        assert!(texts.contains(&("warning", "careful")), "{texts:?}");
        assert!(texts.iter().any(|(level, text)| *level == "exception" && text.starts_with("ReferenceError")), "{texts:?}");
        let failed: Vec<(&str, Option<u16>)> = context.network.iter().map(|n| (n.method.as_str(), n.status)).collect();
        assert_eq!(failed, [("POST", Some(500)), ("GET", Some(404))]);
        assert!(context.network[0].url.ends_with("/api/x?api_key=REDACTED"));
        let _ = std::fs::remove_dir_all(root);
    }
}
