//! Deciding where a capture goes from what was seen around it (`docs/routing.md`).
//!
//! `look` gathers [`Evidence`]; [`decide`] turns it into a [`Judgement`] without reading the
//! clock or doing I/O, so the same evidence always gives the same decision.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[cfg(any(target_os = "macos", test))]
use crate::model::Destination;

/// A send needs at least this score...
pub const SEND_AT: u32 = 95;
/// ...and this lead over the second candidate.
pub const LEAD: u32 = 15;
/// Below this a candidate isn't worth suggesting.
pub const OFFER_AT: u32 = 40;

/// Everything [`decide`] needs. Plain data, saved as the bundle's `routing.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<Address>,
    #[serde(default)]
    pub clues: Vec<Clue>,
    #[serde(default)]
    pub candidates: Vec<Candidate>,
    /// The capture's end, epoch ms; recency is measured from here.
    pub now_ms: u64,
}

/// A destination someone stated. Obeyed, never weighed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "session")]
pub enum Address {
    /// The user picked it (`--session`, a chip).
    Chosen(String),
    /// The recording carries it (a ScrawlKit timeline's `session`, an earlier routing).
    Recorded(String),
}

impl Clue {
    /// The project this clue names, if any.
    fn directory(&self) -> Option<PathBuf> {
        match &self.pointer {
            Pointer::Session { directory, .. } => directory.clone(),
            Pointer::Folder(folder) => Some(folder.root.clone()),
            Pointer::Words { .. } => None,
        }
    }
}

impl Address {
    pub fn session(&self) -> &str {
        match self {
            Self::Chosen(id) | Self::Recorded(id) => id,
        }
    }
}

/// An observed fact pointing at sessions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Clue {
    pub source: Source,
    pub pointer: Pointer,
    /// What was seen, for explanations.
    pub note: String,
}

/// Where a clue came from. Each source counts once per candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    /// The tab registered through `/sdk.js`.
    Page,
    /// The process serving the tab's port on this Mac.
    Port,
    /// The captured terminal's focused pane.
    Terminal,
    /// The captured window's document.
    Document,
    /// The session OpenCode Desktop showed last.
    Viewed,
    /// The captured app's name (an app being built names its project).
    App,
    /// Words in the window or tab title.
    Title,
    /// Words that were said.
    Speech,
}

impl Source {
    /// The `via` written into `scrawl.json`'s `destination`. The ladder's two keep their names.
    #[cfg(any(target_os = "macos", test))]
    pub fn via(self) -> &'static str {
        match self {
            Self::Page => "sdk",
            Self::Port => "localhost",
            Self::Terminal => "terminal",
            Self::Document => "document",
            Self::Viewed => "viewed",
            Self::App => "app",
            Self::Title => "title",
            Self::Speech => "speech",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Page => "page",
            Self::Port => "port",
            Self::Terminal => "terminal",
            Self::Document => "document",
            Self::Viewed => "viewed",
            Self::App => "app",
            Self::Title => "title",
            Self::Speech => "speech",
        }
    }

    /// Full points for a clue of this source that names a candidate.
    fn points(self) -> u32 {
        match self {
            Self::Page | Self::Port => 95,
            Self::Terminal => 70,
            Self::Document | Self::Viewed => 60,
            Self::App => 50,
            Self::Title => 40,
            Self::Speech => 25,
        }
    }
}

/// A page naming its session outright is worth a little more than a folder.
const PAGE_SESSION: u32 = 100;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Pointer {
    /// `directory` is a folder the page also named, kept for the record.
    Session {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        directory: Option<PathBuf>,
    },
    Folder(Folder),
    Words {
        words: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub path: PathBuf,
    /// Its project: the git root, or `path` outside a repository.
    pub root: PathBuf,
    /// Whether sessions anywhere under `root` count (inside a repository).
    pub tree: bool,
    /// The repository's main checkout, shared by its worktrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<PathBuf>,
}

/// An open root session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<PathBuf>,
    #[serde(default)]
    pub updated_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewed_ms: Option<u64>,
}

impl Candidate {
    pub fn label(&self) -> &str {
        if self.title.is_empty() {
            &self.id
        } else {
            &self.title
        }
    }

    /// The words that name it: its title's, and its folder's name split on `-`, `_` and `.`.
    fn words(&self) -> BTreeSet<String> {
        let folder = self
            .directory
            .as_deref()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        words(&format!("{} {folder}", self.title))
            .into_iter()
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// Today's ladder (the app still calls `routing::route_capture` when smart routing is off;
    /// this reproduces it, which the tests check).
    /// The first source with a clue decides, and its newest session wins.
    #[cfg_attr(not(test), allow(dead_code))]
    Ladder,
    /// Weigh every clue and send only when one candidate is clearly ahead.
    Weigh,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Judgement {
    pub decision: Decision,
    /// Supported candidates, best first.
    pub ranking: Vec<Scored>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Decision {
    Send { to: Scored },
    Offer { choices: Vec<Scored> },
    Keep { reason: String },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Scored {
    pub candidate: Candidate,
    pub score: u32,
    pub reasons: Vec<Reason>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reason {
    pub basis: Basis,
    pub points: u32,
    pub why: String,
    /// The project the clue named, recorded as the destination's `directory`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<PathBuf>,
}

/// What a reason rests on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "source")]
pub enum Basis {
    Chosen,
    Recorded,
    Clue(Source),
    Recent,
}

impl Basis {
    fn label(self) -> &'static str {
        match self {
            Self::Chosen => "chosen",
            Self::Recorded => "recorded",
            Self::Clue(source) => source.label(),
            Self::Recent => "recent",
        }
    }
}

impl Scored {
    /// The reason that gave the most points (the first on a tie), which names how it was found.
    #[cfg(any(target_os = "macos", test))]
    fn lead(&self) -> Option<&Reason> {
        self.reasons
            .iter()
            .filter(|r| r.basis != Basis::Recent)
            .fold(None, |best: Option<&Reason>, r| match best {
                Some(b) if b.points >= r.points => Some(b),
                _ => Some(r),
            })
    }
}

/// Where the evidence says the capture goes.
pub fn decide(evidence: &Evidence, policy: Policy) -> Judgement {
    if let Some(address) = &evidence.address {
        let to = addressed(evidence, address);
        return Judgement {
            decision: Decision::Send { to: to.clone() },
            ranking: vec![to],
        };
    }
    match policy {
        Policy::Ladder => ladder(evidence),
        Policy::Weigh => weigh(evidence),
    }
}

fn addressed(evidence: &Evidence, address: &Address) -> Scored {
    let id = address.session();
    let candidate = find(&evidence.candidates, id).unwrap_or_else(|| bare(id));
    let (basis, why) = match address {
        Address::Chosen(_) => (Basis::Chosen, "you chose it"),
        Address::Recorded(_) => (Basis::Recorded, "the recording was addressed to it"),
    };
    Scored {
        candidate,
        score: u32::MAX,
        reasons: vec![Reason {
            basis,
            points: u32::MAX,
            why: why.into(),
            directory: None,
        }],
    }
}

fn find(candidates: &[Candidate], id: &str) -> Option<Candidate> {
    candidates.iter().find(|c| c.id == id).cloned()
}

/// A session known only by its ID (not among the candidates read).
fn bare(id: &str) -> Candidate {
    Candidate {
        id: id.to_owned(),
        title: String::new(),
        directory: None,
        repo: None,
        updated_ms: 0,
        viewed_ms: None,
    }
}

// MARK: The ladder

/// The rungs, in order. Only the first source that has a clue is consulted.
const RUNGS: [Source; 2] = [Source::Page, Source::Port];

fn ladder(evidence: &Evidence) -> Judgement {
    let keep = |reason: String| Judgement {
        decision: Decision::Keep { reason },
        ranking: Vec::new(),
    };
    let Some(clue) = RUNGS
        .iter()
        .find_map(|rung| evidence.clues.iter().find(|c| c.source == *rung))
    else {
        return keep("no page registration or local dev server".into());
    };
    let pick = match &clue.pointer {
        Pointer::Session { id, .. } => {
            Some(find(&evidence.candidates, id).unwrap_or_else(|| bare(id)))
        }
        Pointer::Folder(folder) => newest_in(&evidence.candidates, folder),
        Pointer::Words { .. } => None,
    };
    match pick {
        Some(candidate) => {
            let to = Scored {
                candidate,
                score: clue.source.points(),
                reasons: vec![Reason {
                    basis: Basis::Clue(clue.source),
                    points: clue.source.points(),
                    why: clue.note.clone(),
                    directory: clue.directory(),
                }],
            };
            Judgement {
                decision: Decision::Send { to: to.clone() },
                ranking: vec![to],
            }
        }
        None => keep(match &clue.pointer {
            Pointer::Folder(folder) => {
                format!("no open OpenCode session in {}", folder.root.display())
            }
            _ => clue.note.clone(),
        }),
    }
}

/// The most recently updated candidate in `folder`'s project.
fn newest_in(candidates: &[Candidate], folder: &Folder) -> Option<Candidate> {
    candidates
        .iter()
        .filter(|c| in_project(c, folder))
        .max_by(|a, b| a.updated_ms.cmp(&b.updated_ms).then(b.id.cmp(&a.id)))
        .cloned()
}

pub(crate) fn is_home_or_root(path: &Path) -> bool {
    path == Path::new("/") || dirs::home_dir().is_some_and(|home| path == home)
}

fn in_project(candidate: &Candidate, folder: &Folder) -> bool {
    if is_home_or_root(&folder.root) {
        return false;
    }
    candidate
        .directory
        .as_deref()
        .is_some_and(|dir| dir == folder.root || (folder.tree && dir.starts_with(&folder.root)))
}

// MARK: Weighing

fn weigh(evidence: &Evidence) -> Judgement {
    let candidates = with_named_sessions(evidence);
    let frequency = word_frequency(&candidates);
    // For each folder clue, how close its closest candidate is.
    let closest: Vec<Option<usize>> = evidence
        .clues
        .iter()
        .map(|clue| match &clue.pointer {
            Pointer::Folder(folder) => candidates
                .iter()
                .filter_map(|c| match fit(c, folder) {
                    Fit::Along(d) => Some(d),
                    _ => None,
                })
                .min(),
            _ => None,
        })
        .collect();
    let mut ranking: Vec<Scored> = candidates
        .iter()
        .filter_map(|candidate| score(candidate, evidence, &frequency, &closest))
        .collect();
    ranking.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.candidate.id.cmp(&b.candidate.id))
    });

    let decision = match ranking.as_slice() {
        [] => Decision::Keep {
            reason: if evidence.clues.is_empty() {
                "nothing seen points at a session".into()
            } else {
                "no open session matches what was seen".into()
            },
        },
        [best, rest @ ..] => {
            let second = rest.first();
            let lead = best.score - second.map_or(0, |s| s.score);
            if best.score >= SEND_AT && lead >= LEAD {
                Decision::Send { to: best.clone() }
            } else if best.score >= OFFER_AT {
                let mut choices = vec![best.clone()];
                if let Some(second) = second.filter(|s| s.score >= OFFER_AT && lead < LEAD) {
                    choices.push(second.clone());
                }
                Decision::Offer { choices }
            } else {
                Decision::Keep {
                    reason: format!(
                        "the best match, {}, scores {} (an offer needs {OFFER_AT})",
                        best.candidate.label(),
                        best.score
                    ),
                }
            }
        }
    };
    Judgement { decision, ranking }
}

/// The candidates, plus a bare one for each session a page names that wasn't read.
fn with_named_sessions(evidence: &Evidence) -> Vec<Candidate> {
    let mut candidates = evidence.candidates.clone();
    for clue in &evidence.clues {
        if let (Source::Page, Pointer::Session { id, .. }) = (clue.source, &clue.pointer)
            && !candidates.iter().any(|c| &c.id == id)
        {
            candidates.push(bare(id));
        }
    }
    candidates
}

/// How many candidates each word names.
fn word_frequency(candidates: &[Candidate]) -> BTreeMap<String, u32> {
    let mut frequency = BTreeMap::new();
    for candidate in candidates {
        for word in candidate.words() {
            *frequency.entry(word).or_insert(0) += 1;
        }
    }
    frequency
}

fn score(
    candidate: &Candidate,
    evidence: &Evidence,
    frequency: &BTreeMap<String, u32>,
    closest: &[Option<usize>],
) -> Option<Scored> {
    // The best clue of each source.
    let mut best: BTreeMap<Source, Reason> = BTreeMap::new();
    let names = candidate.words();
    for (clue, closest) in evidence.clues.iter().zip(closest) {
        let Some((points, why)) = support(clue, candidate, &names, frequency, *closest) else {
            continue;
        };
        if points == 0 {
            continue;
        }
        let better = best
            .get(&clue.source)
            .is_none_or(|r| points > r.points || (points == r.points && why < r.why));
        if better {
            best.insert(
                clue.source,
                Reason {
                    basis: Basis::Clue(clue.source),
                    points,
                    why,
                    directory: clue.directory(),
                },
            );
        }
    }
    if best.is_empty() {
        return None;
    }
    let mut reasons: Vec<Reason> = best.into_values().collect();
    reasons.sort_by(|a, b| b.points.cmp(&a.points));
    if let Some((points, why)) = recency(candidate, evidence.now_ms) {
        reasons.push(Reason {
            basis: Basis::Recent,
            points,
            why,
            directory: None,
        });
    }
    Some(Scored {
        candidate: candidate.clone(),
        score: reasons.iter().map(|r| r.points).sum(),
        reasons,
    })
}

/// The points `clue` gives `candidate`, and why.
fn support(
    clue: &Clue,
    candidate: &Candidate,
    names: &BTreeSet<String>,
    frequency: &BTreeMap<String, u32>,
    closest: Option<usize>,
) -> Option<(u32, String)> {
    match &clue.pointer {
        Pointer::Session { id, .. } if *id == candidate.id => {
            let points = if clue.source == Source::Page {
                PAGE_SESSION
            } else {
                clue.source.points()
            };
            Some((points, clue.note.clone()))
        }
        Pointer::Session { .. } => None,
        Pointer::Folder(folder) => {
            let full = clue.source.points();
            match fit(candidate, folder) {
                Fit::Along(d) => {
                    let further = d.saturating_sub(closest.unwrap_or(d));
                    let points = full.saturating_sub(FURTHER * further as u32).max(full / 2);
                    let why = if further == 0 {
                        clue.note.clone()
                    } else {
                        format!(
                            "{} ({further} folder{} further than the closest session)",
                            clue.note,
                            if further == 1 { "" } else { "s" }
                        )
                    };
                    Some((points, why))
                }
                Fit::Aside => Some((
                    full / 2,
                    format!("{} (elsewhere in the same project)", clue.note),
                )),
                Fit::Repo => Some((
                    full / 2,
                    format!("{} (another checkout of the same repository)", clue.note),
                )),
                Fit::None => None,
            }
        }
        Pointer::Words { words } => {
            let (word, n) = words
                .iter()
                .filter(|w| names.contains(*w))
                .map(|w| (w, frequency.get(w).copied().unwrap_or(1).max(1)))
                .min_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(b.0)))?;
            let points = clue.source.points() / n;
            let share = if n == 1 {
                "only this session".to_owned()
            } else {
                format!("{n} sessions")
            };
            Some((points, format!("{}: \"{word}\" names {share}", clue.note)))
        }
    }
}

/// Points a folder clue loses for each folder between a session and the clue's folder, beyond
/// the closest session's distance.
const FURTHER: u32 = 15;

/// How a session's directory sits relative to a folder clue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fit {
    /// On the line from the project root to the folder, or inside the folder: this many
    /// folders away from it.
    Along(usize),
    /// Elsewhere in the project.
    Aside,
    /// In another checkout of the same repository.
    Repo,
    None,
}

fn fit(candidate: &Candidate, folder: &Folder) -> Fit {
    if in_project(candidate, folder) {
        let dir = candidate.directory.as_deref().unwrap_or(Path::new("/"));
        let depth = |p: &Path| p.components().count();
        if folder.path.starts_with(dir) {
            Fit::Along(depth(&folder.path) - depth(dir))
        } else if dir.starts_with(&folder.path) {
            Fit::Along(depth(dir) - depth(&folder.path))
        } else {
            Fit::Aside
        }
    } else if same_repo(candidate, folder) {
        Fit::Repo
    } else {
        Fit::None
    }
}

fn same_repo(candidate: &Candidate, folder: &Folder) -> bool {
    match (&candidate.repo, &folder.repo) {
        (Some(a), Some(b)) => a == b && !is_home_or_root(a),
        _ => false,
    }
}

/// Points for having been active shortly before the capture ended. Activity well after it (a
/// bundle explained later, against today's sessions) says nothing about that moment.
fn recency(candidate: &Candidate, now_ms: u64) -> Option<(u32, String)> {
    const MINUTE: u64 = 60_000;
    let active = [Some(candidate.updated_ms), candidate.viewed_ms]
        .into_iter()
        .flatten()
        .filter(|ms| *ms > 0 && *ms <= now_ms + MINUTE)
        .max()?;
    let ago = now_ms.saturating_sub(active);
    let points = match ago {
        a if a <= 10 * MINUTE => 20,
        a if a <= 60 * MINUTE => 12,
        a if a <= 24 * 60 * MINUTE => 5,
        _ => return None,
    };
    Some((
        points,
        format!("active {} before the capture ended", ago_text(ago)),
    ))
}

pub fn ago_text(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min", s / 60),
        _ if s < 172_800 => format!("{} h", s / 3600),
        _ => format!("{} days", s / 86_400),
    }
}

// MARK: Words

/// Words too common to name a session, whitespace-separated.
const STOP: &str = "the and for but not you your with this that these those there here what when where which who \
     why how just like yeah okay um uh also really maybe should would could can will its are was \
     were been have has had does did doing get got make makes made see look looks thing things \
     some all any one two too very much more most now then than into onto from out off over under \
     about again still even kind sort think know want need going way right left new old app fix \
     fixes work worktree main test tests feat pass they them our let lets bit lot little good \
     nice better weird mean probably actually because something everything page window tab \
     session sessions review preview index html www com http https localhost src code open source \
     users kit only down each different next none quite lots whole often around starts start \
     feels feel sometimes happen happens gonna wanna guys dont doesnt isnt cant wont didnt thats \
     whats theres youre shit fuck damn same other another first last every while after before \
     through being point part parts show shows says said tell take give keep put try use used \
     using time times day back able sure yes delete read write free tier settings setting";

/// Lowercased words of three letters or more that aren't on the stop list or numbers.
pub fn words(text: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    // "don't" is one word.
    let text = text.replace(['\'', '’'], "");
    text.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| w.chars().count() >= 3)
        .filter(|w| !w.chars().all(|c| c.is_ascii_digit()))
        .filter(|w| !STOP.split_whitespace().any(|stop| stop == w))
        .filter(|w| seen.insert(w.clone()))
        .collect()
}

// MARK: Effects

/// The `destination` a send records, or `None` when nothing is sent.
#[cfg(any(target_os = "macos", test))]
pub fn destination(judgement: &Judgement, url: Option<&str>) -> Option<Destination> {
    let Decision::Send { to } = &judgement.decision else {
        return None;
    };
    let lead = to.lead();
    let via = match lead.map(|r| r.basis) {
        Some(Basis::Chosen) => "chosen",
        Some(Basis::Recorded) => "recorded",
        Some(Basis::Clue(source)) => source.via(),
        Some(Basis::Recent) | None => "weighed",
    };
    Some(Destination {
        session: to.candidate.id.clone(),
        title: to.candidate.label().to_owned(),
        via: via.to_owned(),
        url: url.map(str::to_owned),
        directory: lead
            .and_then(|r| r.directory.as_ref())
            .map(|d| d.display().to_string()),
    })
}

// MARK: Explaining

/// A plain-text account of the evidence, the scores and the decision.
pub fn explain(evidence: &Evidence, judgement: &Judgement) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    match &evidence.address {
        Some(Address::Chosen(id)) => {
            let _ = writeln!(out, "Address   chosen: {id}");
        }
        Some(Address::Recorded(id)) => {
            let _ = writeln!(out, "Address   recorded: {id}");
        }
        None => {
            let _ = writeln!(out, "Address   none");
        }
    }
    if evidence.clues.is_empty() {
        let _ = writeln!(out, "Clues     none");
    } else {
        let _ = writeln!(out, "Clues");
        for clue in &evidence.clues {
            let target = match &clue.pointer {
                Pointer::Session { id, .. } => format!("session {id}"),
                Pointer::Folder(f) if f.tree => format!("project {}", f.root.display()),
                Pointer::Folder(f) => format!("folder {}", f.root.display()),
                Pointer::Words { words } => {
                    let shown: Vec<&str> = words.iter().take(12).map(String::as_str).collect();
                    let more = words.len().saturating_sub(shown.len());
                    if more > 0 {
                        format!("words {} (+{more})", shown.join(", "))
                    } else {
                        format!("words {}", shown.join(", "))
                    }
                }
            };
            let _ = writeln!(out, "  {:<9} {} → {target}", clue.source.label(), clue.note);
        }
    }
    let _ = writeln!(
        out,
        "Sessions  {} read, {} supported",
        evidence.candidates.len(),
        judgement.ranking.len()
    );
    for scored in judgement.ranking.iter().take(6) {
        let score = if scored.score == u32::MAX {
            "  ∞".to_owned()
        } else {
            format!("{:>4}", scored.score)
        };
        let dir = scored
            .candidate
            .directory
            .as_ref()
            .map(|d| format!("  {}", d.display()))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "{score}  {}  {}{dir}",
            scored.candidate.label(),
            scored.candidate.id
        );
        for reason in &scored.reasons {
            let label = reason.basis.label();
            let points = if reason.points == u32::MAX {
                String::new()
            } else {
                format!("+{} ", reason.points)
            };
            let _ = writeln!(out, "        {points}{label} {}", reason.why);
        }
    }
    let _ = match &judgement.decision {
        Decision::Send { to } => writeln!(
            out,
            "Decision  send to {} ({})",
            to.candidate.label(),
            to.candidate.id
        ),
        Decision::Offer { choices } => {
            let names: Vec<&str> = choices.iter().map(|c| c.candidate.label()).collect();
            let best = choices.first().map_or(0, |c| c.score);
            writeln!(
                out,
                "Decision  offer {} (a send needs {SEND_AT} and a lead of {LEAD}; best is {best})",
                names.join(" or ")
            )
        }
        Decision::Keep { reason } => writeln!(out, "Decision  clipboard only: {reason}"),
    };
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;
    const MIN: u64 = 60_000;

    fn cand(id: &str, title: &str, dir: &str, ago_min: u64) -> Candidate {
        Candidate {
            id: id.into(),
            title: title.into(),
            directory: Some(dir.into()),
            repo: None,
            updated_ms: NOW - ago_min * MIN,
            viewed_ms: None,
        }
    }

    fn folder(source: Source, root: &str) -> Clue {
        Clue {
            source,
            pointer: Pointer::Folder(Folder {
                path: root.into(),
                root: root.into(),
                tree: true,
                repo: None,
            }),
            note: format!("{source:?} in {root}"),
        }
    }

    fn named(source: Source, id: &str) -> Clue {
        Clue {
            source,
            pointer: Pointer::Session {
                id: id.into(),
                directory: None,
            },
            note: format!("{source:?} names {id}"),
        }
    }

    fn said(source: Source, text: &str) -> Clue {
        Clue {
            source,
            pointer: Pointer::Words { words: words(text) },
            note: format!("{source:?}"),
        }
    }

    fn sessions() -> Vec<Candidate> {
        vec![
            cand("ses_scrawl", "SCRAWL", "/p/scrawl", 2),
            cand("ses_ios", "OPENCODE IOS", "/p/opencode-ios", 5),
            cand("ses_console", "CONSOLE VORTEX", "/p/opencode-console", 30),
            cand("ses_old", "SCRAWL OLD", "/p/scrawl", 60 * 24 * 5),
        ]
    }

    fn ev(clues: Vec<Clue>) -> Evidence {
        Evidence {
            address: None,
            clues,
            candidates: sessions(),
            now_ms: NOW,
        }
    }

    fn sent_to(j: &Judgement) -> Option<&str> {
        match &j.decision {
            Decision::Send { to } => Some(to.candidate.id.as_str()),
            _ => None,
        }
    }

    fn offered(j: &Judgement) -> Vec<&str> {
        match &j.decision {
            Decision::Offer { choices } => {
                choices.iter().map(|c| c.candidate.id.as_str()).collect()
            }
            _ => Vec::new(),
        }
    }

    #[test]
    fn an_address_always_wins() {
        let mut e = ev(vec![
            named(Source::Page, "ses_ios"),
            folder(Source::Port, "/p/scrawl"),
        ]);
        e.address = Some(Address::Recorded("ses_console".into()));
        for policy in [Policy::Ladder, Policy::Weigh] {
            assert_eq!(sent_to(&decide(&e, policy)), Some("ses_console"));
        }
        // An unknown session is still obeyed.
        e.address = Some(Address::Chosen("ses_gone".into()));
        let j = decide(&e, Policy::Weigh);
        assert_eq!(sent_to(&j), Some("ses_gone"));
        assert_eq!(destination(&j, None).unwrap().via, "chosen");
    }

    #[test]
    fn a_page_or_port_sends_on_its_own() {
        let j = decide(&ev(vec![named(Source::Page, "ses_ios")]), Policy::Weigh);
        assert_eq!(sent_to(&j), Some("ses_ios"));
        assert_eq!(destination(&j, Some("http://x")).unwrap().via, "sdk");
        // The port names /p/scrawl: the recent session wins over the five-day-old one.
        let j = decide(&ev(vec![folder(Source::Port, "/p/scrawl")]), Policy::Weigh);
        assert_eq!(sent_to(&j), Some("ses_scrawl"));
        assert_eq!(destination(&j, None).unwrap().via, "localhost");
    }

    #[test]
    fn two_close_sessions_are_offered_not_sent() {
        let mut e = ev(vec![folder(Source::Port, "/p/scrawl")]);
        e.candidates[3].updated_ms = NOW - 3 * MIN;
        let j = decide(&e, Policy::Weigh);
        assert_eq!(sent_to(&j), None);
        assert_eq!(offered(&j), vec!["ses_old", "ses_scrawl"]);
        // The ladder sends to the newest regardless.
        assert_eq!(sent_to(&decide(&e, Policy::Ladder)), Some("ses_scrawl"));
    }

    #[test]
    fn whats_on_screen_is_a_hint_not_an_address() {
        let mut e = ev(vec![named(Source::Viewed, "ses_ios")]);
        e.candidates[1].viewed_ms = Some(NOW);
        let j = decide(&e, Policy::Weigh);
        assert_eq!(offered(&j), vec!["ses_ios"]);
        // A second, independent clue that agrees sends.
        e.clues
            .push(said(Source::Speech, "the iOS composer feels slow"));
        assert_eq!(sent_to(&decide(&e, Policy::Weigh)), Some("ses_ios"));
    }

    #[test]
    fn a_medium_clue_alone_never_sends() {
        for source in [
            Source::Terminal,
            Source::Document,
            Source::Viewed,
            Source::App,
            Source::Title,
            Source::Speech,
        ] {
            let clue = match source {
                Source::App | Source::Title | Source::Speech => said(source, "scrawl"),
                Source::Viewed => named(source, "ses_scrawl"),
                _ => folder(source, "/p/scrawl"),
            };
            let mut e = ev(vec![clue]);
            e.candidates.truncate(3);
            assert_eq!(sent_to(&decide(&e, Policy::Weigh)), None, "{source:?}");
        }
    }

    #[test]
    fn recency_never_nominates() {
        let j = decide(&ev(vec![folder(Source::Port, "/p/nowhere")]), Policy::Weigh);
        assert!(j.ranking.is_empty());
        assert!(matches!(j.decision, Decision::Keep { .. }));
        let j = decide(&ev(Vec::new()), Policy::Weigh);
        assert!(matches!(j.decision, Decision::Keep { .. }));
    }

    #[test]
    fn decisions_ignore_order_and_repeats() {
        let clues = vec![
            folder(Source::Terminal, "/p/opencode-ios"),
            said(Source::Speech, "composer in opencode ios"),
            named(Source::Viewed, "ses_ios"),
            said(Source::Title, "Scrawl"),
        ];
        let base = decide(&ev(clues.clone()), Policy::Weigh);
        let mut reversed = ev(clues.iter().rev().cloned().collect());
        reversed.candidates.reverse();
        assert_eq!(decide(&reversed, Policy::Weigh), base);
        let mut doubled = clues.clone();
        doubled.extend(clues);
        assert_eq!(decide(&ev(doubled), Policy::Weigh), base);
    }

    #[test]
    fn support_never_hurts_the_leader() {
        let mut e = ev(vec![folder(Source::Port, "/p/opencode-ios")]);
        assert_eq!(sent_to(&decide(&e, Policy::Weigh)), Some("ses_ios"));
        for extra in [
            named(Source::Viewed, "ses_ios"),
            said(Source::Speech, "ios"),
            folder(Source::Terminal, "/p/opencode-ios"),
        ] {
            e.clues.push(extra);
            assert_eq!(sent_to(&decide(&e, Policy::Weigh)), Some("ses_ios"));
        }
    }

    #[test]
    fn home_is_not_a_project() {
        let home = dirs::home_dir().unwrap();
        let mut e = ev(vec![folder(Source::Port, &home.to_string_lossy())]);
        e.candidates
            .push(cand("ses_home", "HOME", &home.to_string_lossy(), 1));
        assert!(decide(&e, Policy::Weigh).ranking.is_empty());
        assert!(
            decide(&ev(vec![folder(Source::Port, "/")]), Policy::Weigh)
                .ranking
                .is_empty()
        );
    }

    #[test]
    fn rare_words_count_more_than_common_ones() {
        let e = ev(vec![said(Source::Speech, "opencode console is broken")]);
        let j = decide(&e, Policy::Weigh);
        // "console" names one session (25 + 5 recency); "opencode" names two.
        assert_eq!(j.ranking[0].candidate.id, "ses_console");
        assert_eq!(j.ranking[0].score, 25 + 12);
        assert!(
            matches!(j.decision, Decision::Offer { .. })
                || matches!(j.decision, Decision::Keep { .. })
        );
    }

    #[test]
    fn the_session_closest_to_the_served_folder_wins() {
        // A dev server in a subfolder of a big repository with many sessions at its root.
        let mut e = ev(Vec::new());
        e.candidates = vec![
            cand("ses_root_a", "HOME", "/h", 3),
            cand("ses_root_b", "NOTES", "/h", 4),
            cand("ses_motion", "KIT MOTION", "/h/research/motion", 30),
            cand("ses_tools", "TOOLS", "/h/tools", 1),
        ];
        e.clues.push(Clue {
            source: Source::Port,
            pointer: Pointer::Folder(Folder {
                path: "/h/research/motion".into(),
                root: "/h".into(),
                tree: true,
                repo: Some("/h".into()),
            }),
            note: "port 5203".into(),
        });
        let j = decide(&e, Policy::Weigh);
        assert_eq!(sent_to(&j), Some("ses_motion"));
        let points = |id: &str| {
            j.ranking
                .iter()
                .find(|s| s.candidate.id == id)
                .unwrap()
                .score
        };
        assert_eq!(points("ses_motion"), 95 + 12);
        assert_eq!(points("ses_root_a"), 65 + 20);
        assert_eq!(points("ses_tools"), 47 + 20);
        // With only root sessions, the root is the closest and sends as today.
        e.candidates.retain(|c| c.id == "ses_root_a");
        assert_eq!(sent_to(&decide(&e, Policy::Weigh)), Some("ses_root_a"));
    }

    #[test]
    fn another_checkout_of_the_repo_gets_half() {
        let mut e = ev(Vec::new());
        e.candidates[0].repo = Some("/p/scrawl".into());
        e.clues.push(Clue {
            source: Source::Port,
            pointer: Pointer::Folder(Folder {
                path: "/w/scrawl-routing/site".into(),
                root: "/w/scrawl-routing".into(),
                tree: true,
                repo: Some("/p/scrawl".into()),
            }),
            note: "port 5173".into(),
        });
        let j = decide(&e, Policy::Weigh);
        assert_eq!(offered(&j), vec!["ses_scrawl"]);
        assert_eq!(j.ranking[0].score, 47 + 20);
    }

    #[test]
    fn the_ladder_takes_the_first_rung_with_a_clue() {
        // A page folder with no session doesn't fall through to the port.
        let e = ev(vec![
            folder(Source::Port, "/p/scrawl"),
            folder(Source::Page, "/p/none"),
        ]);
        let j = decide(&e, Policy::Ladder);
        assert!(matches!(&j.decision, Decision::Keep { reason } if reason.contains("/p/none")));
        // Other sources are ignored.
        let e = ev(vec![named(Source::Viewed, "ses_ios")]);
        assert!(matches!(
            decide(&e, Policy::Ladder).decision,
            Decision::Keep { .. }
        ));
    }

    #[test]
    fn words_skip_filler_and_keep_names() {
        assert_eq!(
            words("Why don't we fix the Scrawl pill, who knows? OpenCode iOS 26"),
            vec!["scrawl", "pill", "knows", "opencode", "ios"]
        );
    }

    #[test]
    fn evidence_round_trips_through_json() {
        let mut e = ev(vec![
            folder(Source::Port, "/p/scrawl"),
            named(Source::Page, "ses_ios"),
            said(Source::Speech, "hello scrawl"),
        ]);
        e.address = Some(Address::Recorded("ses_ios".into()));
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<Evidence>(&json).unwrap(), e);
    }
}
