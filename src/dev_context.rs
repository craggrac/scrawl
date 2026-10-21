//! What the capture's surroundings said while it recorded, for the agent that reads it: the
//! focused browser tab's console errors, failed requests and page changes (Chrome DevTools
//! Protocol), and the repository the terminal or the dev server was in. Collected on Linux
//! (`crate::linux::context`), stored in `scrawl.json` as `devContext` and shown compactly under
//! `Context:` in `summary.md`. Times are on the capture clock; every string is already redacted
//! and cut by the collector.

use serde::{Deserialize, Serialize};

use crate::model::format_clock;

/// The most timeline lines `summary.md` shows; the rest stay in `scrawl.json`.
const SUMMARY_TIMELINE_LINES: usize = 12;
/// The most repositories `summary.md` shows.
const SUMMARY_PROJECT_LINES: usize = 3;
/// Changed files named on a repository's summary line.
const SUMMARY_CHANGED_FILES: usize = 4;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevContext {
    /// The focused tab's address when the capture started and each time it changed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<PageVisit>,
    /// Console errors and warnings and uncaught exceptions of the focused tab.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub console: Vec<ConsoleEntry>,
    /// Requests of the focused tab that failed or answered 400 and above.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network: Vec<NetworkFailure>,
    /// Folders the terminal in front or the page's dev server ran in, and their repositories.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<Project>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageVisit {
    pub t: u64,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleEntry {
    pub t: u64,
    /// `"error"`, `"warning"` or `"exception"` (uncaught).
    pub level: String,
    pub text: String,
    /// The top stack frame, `file.js:12:5`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// How many times the same message came, back to back: `t` is the first.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkFailure {
    pub t: u64,
    pub method: String,
    /// Without credentials, query secrets or fragment secrets.
    pub url: String,
    /// The HTTP status; none when the request never got an answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// The browser's reason when there was no answer (`net::ERR_CONNECTION_REFUSED`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// How many times the same request failed the same way: `t` is the first.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    /// When it was first seen.
    pub t: u64,
    /// The working directory found.
    pub dir: String,
    /// `"terminal"`, or `"localhost:PORT"` for the process serving the page.
    pub source: String,
    /// The git root, when `dir` is in a repository; the fields below are then set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Short commit id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// The commit's subject line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Entries of `git status --porcelain`; none when it wasn't run (the repository's own
    /// configuration names filter programs, which a status could start).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<u32>,
    /// The first of them, `"M src/main.rs"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

fn one() -> u32 {
    1
}

fn is_one(n: &u32) -> bool {
    *n == 1
}

impl DevContext {
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
            && self.console.is_empty()
            && self.network.is_empty()
            && self.projects.is_empty()
    }

    /// The `Context:` block of `summary.md` (empty when there is nothing): browser events and
    /// page changes in time order, then the repositories. `home` shortens folders to `~`.
    pub fn summary_lines(&self, home: Option<&str>) -> Vec<String> {
        let mut timeline: Vec<(u64, String)> = Vec::new();
        for (i, page) in self.pages.iter().enumerate() {
            // The first address is where the capture started; naming the host and path is
            // enough, the query was cut.
            let arrow = if i == 0 { "page" } else { "page →" };
            timeline.push((page.t, format!("{arrow} {}", short_url(&page.url))));
        }
        for entry in &self.console {
            let what = match entry.level.as_str() {
                "exception" => "uncaught",
                "warning" => "console warning:",
                _ => "console error:",
            };
            let at = entry.at.as_deref().map_or(String::new(), |at| format!(" ({at})"));
            timeline.push((
                entry.t,
                format!("{what} {}{at}{}", entry.text, repeats(entry.count)),
            ));
        }
        for failure in &self.network {
            let host = self.page_host_at(failure.t);
            let result = match (failure.status, &failure.error) {
                (Some(status), _) => status.to_string(),
                (None, Some(error)) => format!("failed ({error})"),
                (None, None) => "failed".to_owned(),
            };
            timeline.push((
                failure.t,
                format!(
                    "{} {} → {result}{}",
                    failure.method,
                    request_target(&failure.url, host.as_deref()),
                    repeats(failure.count)
                ),
            ));
        }
        timeline.sort_by_key(|(t, _)| *t);
        let mut lines: Vec<String> = timeline
            .iter()
            .take(SUMMARY_TIMELINE_LINES)
            .map(|(t, text)| format!("{} {text}", format_clock(*t)))
            .collect();
        if timeline.len() > SUMMARY_TIMELINE_LINES {
            lines.push(format!(
                "+{} more in scrawl.json",
                timeline.len() - SUMMARY_TIMELINE_LINES
            ));
        }
        lines.extend(
            self.projects
                .iter()
                .take(SUMMARY_PROJECT_LINES)
                .map(|project| project.summary_line(home)),
        );
        lines
    }

    /// The host of the page that was showing at `t`.
    fn page_host_at(&self, t: u64) -> Option<String> {
        let page = self.pages.iter().rev().find(|page| page.t <= t)?;
        Some(split_url(&page.url).0.to_owned())
    }
}

impl Project {
    fn summary_line(&self, home: Option<&str>) -> String {
        let shown = |path: &str| match home {
            Some(home) if path == home => "~".to_owned(),
            Some(home) => path
                .strip_prefix(home)
                .filter(|rest| rest.starts_with('/'))
                .map_or_else(|| path.to_owned(), |rest| format!("~{rest}")),
            None => path.to_owned(),
        };
        let Some(root) = &self.root else {
            return format!("cwd {} ({})", shown(&self.dir), self.source);
        };
        let name = root.rsplit('/').find(|part| !part.is_empty()).unwrap_or(root);
        let mut line = format!("repo {name}");
        if let Some(branch) = &self.branch {
            line.push_str(&format!(" @ {branch}"));
        }
        if let Some(head) = &self.head {
            line.push_str(&format!(" {head}"));
        }
        if let Some(subject) = &self.subject {
            line.push_str(&format!(" “{subject}”"));
        }
        match self.changed {
            None => {}
            Some(0) => line.push_str(" (clean)"),
            Some(n) => {
                let named: Vec<&str> = self
                    .files
                    .iter()
                    .take(SUMMARY_CHANGED_FILES)
                    .map(String::as_str)
                    .collect();
                let more = n as usize - named.len().min(n as usize);
                line.push_str(&format!(" ({n} changed: {}", named.join(", ")));
                if more > 0 {
                    line.push_str(&format!(", +{more}"));
                }
                line.push(')');
            }
        }
        line
    }
}

fn repeats(count: u32) -> String {
    if count > 1 {
        format!(" ×{count}")
    } else {
        String::new()
    }
}

/// `(host, path)` of an `http(s)` URL; a string that isn't one is all path.
fn split_url(url: &str) -> (&str, &str) {
    let Some((_, rest)) = url.split_once("://") else {
        return ("", url);
    };
    match rest.find(['/', '?', '#']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    }
}

/// A page address as `host/path`, query and fragment left out.
fn short_url(url: &str) -> String {
    let (host, rest) = split_url(url);
    let path = rest.split(['?', '#']).next().unwrap_or("");
    format!("{host}{}", if path.is_empty() { "/" } else { path })
}

/// A failed request's path, with its host only when it isn't the page's own.
fn request_target(url: &str, page_host: Option<&str>) -> String {
    let (host, rest) = split_url(url);
    let path = rest.split(['?', '#']).next().unwrap_or("");
    let path = if path.is_empty() { "/" } else { path };
    if host.is_empty() || Some(host) == page_host {
        path.to_owned()
    } else {
        format!("{host}{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> DevContext {
        DevContext {
            pages: vec![
                PageVisit {
                    t: 0,
                    url: "http://localhost:3000/dashboard?tab=2".into(),
                },
                PageVisit {
                    t: 12_000,
                    url: "http://localhost:3000/settings".into(),
                },
            ],
            console: vec![
                ConsoleEntry {
                    t: 7_000,
                    level: "exception".into(),
                    text: "TypeError: x is not a function".into(),
                    at: Some("app.js:12:5".into()),
                    count: 1,
                },
                ConsoleEntry {
                    t: 9_500,
                    level: "warning".into(),
                    text: "deprecated".into(),
                    at: None,
                    count: 3,
                },
            ],
            network: vec![
                NetworkFailure {
                    t: 9_000,
                    method: "POST".into(),
                    url: "http://localhost:3000/api/x".into(),
                    status: Some(500),
                    error: None,
                    count: 1,
                },
                NetworkFailure {
                    t: 10_000,
                    method: "GET".into(),
                    url: "https://cdn.example.com/lib.js".into(),
                    status: None,
                    error: Some("net::ERR_CONNECTION_REFUSED".into()),
                    count: 1,
                },
            ],
            projects: vec![Project {
                t: 0,
                dir: "/home/ksm/scrawl/src".into(),
                source: "terminal".into(),
                root: Some("/home/ksm/scrawl".into()),
                branch: Some("linux-port".into()),
                head: Some("42be30e".into()),
                subject: Some("Linux Settings: the Video export switch".into()),
                changed: Some(6),
                files: vec!["M src/a.rs".into(), "?? b.txt".into()],
            }],
        }
    }

    #[test]
    fn the_summary_lists_browser_events_in_time_order_then_the_repository() {
        assert_eq!(
            context().summary_lines(Some("/home/ksm")),
            vec![
                "0:00 page localhost:3000/dashboard",
                "0:07 uncaught TypeError: x is not a function (app.js:12:5)",
                "0:09 POST /api/x → 500",
                "0:09 console warning: deprecated ×3",
                "0:10 GET cdn.example.com/lib.js → failed (net::ERR_CONNECTION_REFUSED)",
                "0:12 page → localhost:3000/settings",
                "repo scrawl @ linux-port 42be30e “Linux Settings: the Video export switch” \
                 (6 changed: M src/a.rs, ?? b.txt, +4)",
            ]
        );
    }

    #[test]
    fn a_long_timeline_is_cut_and_says_where_the_rest_is() {
        let mut context = DevContext::default();
        context.console = (0..20)
            .map(|i| ConsoleEntry {
                t: i * 1_000,
                level: "error".into(),
                text: format!("e{i}"),
                at: None,
                count: 1,
            })
            .collect();
        let lines = context.summary_lines(None);
        assert_eq!(lines.len(), SUMMARY_TIMELINE_LINES + 1);
        assert_eq!(lines.last().unwrap(), "+8 more in scrawl.json");
    }

    #[test]
    fn a_clean_repository_and_a_plain_folder() {
        let mut project = context().projects.remove(0);
        project.changed = Some(0);
        project.files.clear();
        assert!(project.summary_line(None).ends_with("(clean)"));
        let folder = Project {
            root: None,
            dir: "/home/ksm/notes".into(),
            source: "localhost:8080".into(),
            ..project
        };
        assert_eq!(
            folder.summary_line(Some("/home/ksm")),
            "cwd ~/notes (localhost:8080)"
        );
    }

    #[test]
    fn nothing_collected_writes_nothing() {
        assert!(DevContext::default().is_empty());
        assert!(DevContext::default().summary_lines(None).is_empty());
        assert_eq!(
            serde_json::to_string(&DevContext::default()).unwrap(),
            "{}"
        );
    }

    #[test]
    fn it_round_trips_through_json() {
        let json = serde_json::to_string(&context()).unwrap();
        assert!(json.contains("\"changed\":6"));
        assert!(!json.contains("\"count\":1"));
        assert_eq!(serde_json::from_str::<DevContext>(&json).unwrap(), context());
    }
}
