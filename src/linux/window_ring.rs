//! A rolling window log for the replay buffer. A recording has a window log from its start to
//! its end; the buffer has no start, so this keeps the last stretch of one: a sample of the
//! windows on screen about every second (two Hyprland socket requests: the same
//! `hypr::sample_windows` a recording's logger takes), kept only when the list changed, and pruned to the buffer's
//! window. A save cuts the stretch it needs out of it (`WindowRing::finish`) and hands that to
//! the normal finish, whose redaction (`redact`) then hides the windows `redactApps` names in
//! the clip like in any capture.
//!
//! A log that doesn't cover the clip has to be noticed, not trusted: a sample that fails leaves
//! a gap, and `Slice::complete` says whether any gap or missing start/end overlaps the clip.
//! `redact::replay_windows` decides what to do about that.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use crate::model::TargetFrame;
use crate::windows::{WindowEntry, WindowSnapshot};

/// The time between two looks at the windows.
pub const PERIOD: Duration = Duration::from_secs(1);
/// The longest stretch without a good sample that still counts as covered: a missed look or two
/// (a slow socket, a busy compositor), not more.
const MAX_GAP: Duration = Duration::from_secs(3);

/// What the ring holds, in wall-clock time (the buffer's segments are timed the same way).
#[derive(Default)]
pub struct RollingLog {
    /// Samples whose window list differs from the one before (and the look before each change),
    /// oldest first.
    samples: Vec<(SystemTime, Vec<WindowEntry>)>,
    /// When the oldest sample still held was taken, kept apart so pruning can't hide a log that
    /// began late.
    first_ok: Option<SystemTime>,
    /// The latest good look, kept even when it equals the one before (and so isn't in `samples`):
    /// when the list changes it is stored too, so a window that appears is placed in the second
    /// before the change, not anywhere since the last change.
    latest: Option<(SystemTime, Vec<WindowEntry>)>,
    last_ok: Option<SystemTime>,
    /// Stretches between two good samples further apart than `MAX_GAP`.
    gaps: Vec<(SystemTime, SystemTime)>,
}

/// The part of the log a clip needs, on the clip's clock.
#[derive(Debug, Default, PartialEq)]
pub struct Slice {
    /// `t` in milliseconds from the clip's start; the first is at 0 (the state the clip opens on).
    pub snapshots: Vec<WindowSnapshot>,
    /// Whether the log covers the clip from its first to its last moment (no gap, no late start
    /// or early end).
    pub complete: bool,
}

impl RollingLog {
    /// Stores what a look at `at` saw; `None` (the look failed) stores nothing, and the next
    /// good one records the gap.
    pub fn record(&mut self, at: SystemTime, windows: Option<Vec<WindowEntry>>) {
        let Some(windows) = windows else {
            return;
        };
        if let Some(last) = self.last_ok
            && at.duration_since(last).is_ok_and(|d| d > MAX_GAP)
        {
            self.gaps.push((last, at));
        }
        self.first_ok.get_or_insert(at);
        self.last_ok = Some(at);
        if self.samples.last().is_none_or(|(_, held)| *held != windows) {
            if let Some(before) = self.latest.take()
                && self.samples.last().is_some_and(|(held_at, _)| *held_at < before.0)
            {
                self.samples.push(before);
            }
            self.samples.push((at, windows.clone()));
        }
        self.latest = Some((at, windows));
    }

    /// Drops what is older than `before`, except the last sample before it (the state the
    /// stretch after it began in).
    pub fn prune(&mut self, before: SystemTime) {
        let held = self.samples.partition_point(|(at, _)| *at <= before);
        self.samples.drain(..held.saturating_sub(1));
        self.gaps.retain(|(_, end)| *end > before);
        if let Some((at, _)) = self.samples.first()
            && *at <= before
        {
            // What is kept reaches back to `before`: the log did not begin late.
            self.first_ok = Some(*at);
        }
    }

    /// The log for a clip from `start` to `end`.
    pub fn slice(&self, start: SystemTime, end: SystemTime) -> Slice {
        let millis = |at: SystemTime| at.duration_since(start).map_or(0, |d| d.as_millis() as u64);
        let opening = self.samples.partition_point(|(at, _)| *at <= start).saturating_sub(1);
        let snapshots: Vec<WindowSnapshot> = self.samples[opening..]
            .iter()
            .take_while(|(at, _)| *at <= end)
            .map(|(at, windows)| WindowSnapshot { t: millis(*at), windows: windows.clone() })
            .collect();
        let covered = |a: SystemTime, b: SystemTime| b.duration_since(a).is_ok_and(|d| d <= MAX_GAP);
        let complete = match (self.first_ok, self.last_ok) {
            (Some(first), Some(last)) => {
                // The log starts no later than a gap's length after the clip, ends no earlier
                // than that before its end, and no hole lies inside it.
                (first <= start || covered(start, first))
                    && (last >= end || covered(last, end))
                    && !self.gaps.iter().any(|(a, b)| *b > start && *a < end)
            }
            _ => false,
        };
        Slice { snapshots, complete }
    }
}

type Sampler = dyn Fn() -> Option<Vec<WindowEntry>> + Send + Sync;

/// The sampling thread and its log. Dropping it stops the thread (without waiting for a look
/// that is in progress).
pub struct WindowRing {
    log: Arc<Mutex<RollingLog>>,
    sampler: Arc<Sampler>,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

fn lock(log: &Mutex<RollingLog>) -> std::sync::MutexGuard<'_, RollingLog> {
    log.lock().unwrap_or_else(PoisonError::into_inner)
}

impl WindowRing {
    /// Samples the windows of the monitor holding `frame` (a monitor's `monitor_frame`).
    pub fn start(frame: TargetFrame) -> Self {
        Self::start_with(super::hypr::sample_windows_for(frame), PERIOD)
    }

    /// The ring with its own way of looking, and its own period.
    pub fn start_with(
        sampler: impl Fn() -> Option<Vec<WindowEntry>> + Send + Sync + 'static,
        period: Duration,
    ) -> Self {
        let log = Arc::new(Mutex::new(RollingLog::default()));
        let sampler: Arc<Sampler> = Arc::new(sampler);
        let stopping = Arc::new(AtomicBool::new(false));
        let worker = {
            let (log, sampler, stopping) = (Arc::clone(&log), Arc::clone(&sampler), Arc::clone(&stopping));
            thread::Builder::new()
                .name("scrawl-replay-windows".into())
                .spawn(move || {
                    while !stopping.load(Ordering::Acquire) {
                        let seen = sampler();
                        lock(&log).record(SystemTime::now(), seen);
                        thread::park_timeout(period);
                    }
                })
                .ok()
        };
        Self { log, sampler, stopping, worker }
    }

    /// Drops what the buffer's window no longer needs.
    pub fn prune(&self, before: SystemTime) {
        lock(&self.log).prune(before);
    }

    /// The log for the clip `start` to `end` (now), with one last look so the end is not a
    /// second old.
    pub fn finish(&self, start: SystemTime, end: SystemTime) -> Slice {
        let seen = (self.sampler)();
        let mut log = lock(&self.log);
        log.record(end, seen);
        log.slice(start, end)
    }
}

impl Drop for WindowRing {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn at(secs: f64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000) + Duration::from_secs_f64(secs)
    }

    fn win(id: u32) -> WindowEntry {
        WindowEntry {
            id,
            app: format!("app{id}"),
            bundle_id: None,
            regular: true,
            title: String::new(),
            bounds: [0, 0, 100, 100],
            layer: 0,
            frontmost: false,
        }
    }

    fn lists(slice: &Slice) -> Vec<(u64, Vec<u32>)> {
        slice
            .snapshots
            .iter()
            .map(|s| (s.t, s.windows.iter().map(|w| w.id).collect()))
            .collect()
    }

    /// A look every second from `from` to `to` that sees `seen(t)`.
    fn watched(from: u32, to: u32, seen: impl Fn(u32) -> Vec<u32>) -> RollingLog {
        let mut log = RollingLog::default();
        for t in from..=to {
            log.record(at(f64::from(t)), Some(seen(t).into_iter().map(win).collect()));
        }
        log
    }

    #[test]
    fn only_changes_are_kept_and_the_clip_opens_on_the_state_it_began_in() {
        let log = watched(0, 40, |t| if t < 12 { vec![1] } else if t < 30 { vec![1, 2] } else { vec![1] });
        let slice = log.slice(at(20.0), at(40.0));
        // Not one entry per second: the state the clip opens on (at 0: it began before the clip),
        // the last look before the change, and the change.
        assert_eq!(lists(&slice), [(0, vec![1, 2]), (9_000, vec![1, 2]), (10_000, vec![1])]);
        assert!(slice.complete);
        // A clip that starts later than the last change opens on the latest state alone.
        assert_eq!(lists(&log.slice(at(35.0), at(40.0))), [(0, vec![1])]);
    }

    #[test]
    fn pruning_keeps_the_state_the_kept_part_began_in_and_nothing_older() {
        let mut log = watched(0, 100, |t| if t % 20 < 10 { vec![1] } else { vec![1, 2] });
        let held = log.samples.len();
        log.prune(at(55.0));
        assert!(log.samples.len() < held);
        // 50..60 is [1, 2]... t%20: 50 -> 10 -> [1,2]; the sample at 50 is the last one before 55.
        assert_eq!(log.samples[0].0, at(50.0));
        let slice = log.slice(at(55.0), at(100.0));
        assert_eq!(lists(&slice)[0], (0, vec![1, 2]));
        assert!(slice.complete, "a pruned log still covers what it kept");
        assert!(log.gaps.is_empty());
    }

    #[test]
    fn a_failed_look_is_a_gap_only_when_it_is_more_than_a_missed_sample_or_two() {
        let mut log = RollingLog::default();
        for t in [0.0, 1.0, 2.0, 4.5, 5.5] {
            log.record(at(t), Some(vec![win(1)]));
        }
        assert!(log.slice(at(0.0), at(5.5)).complete, "2.5 s is two missed looks");
        log.record(at(10.0), Some(vec![win(1)]));
        assert!(!log.slice(at(0.0), at(10.0)).complete, "4.5 s is a hole");
        assert!(log.slice(at(10.0), at(10.0)).complete, "after the hole it is whole again");
        assert!(log.slice(at(0.0), at(5.0)).complete, "a clip before the hole doesn't touch it");
    }

    #[test]
    fn a_log_that_began_late_or_ended_early_does_not_cover_the_clip() {
        let log = watched(10, 20, |_| vec![1]);
        assert!(!log.slice(at(0.0), at(20.0)).complete, "the first 10 s are unknown");
        assert!(log.slice(at(8.0), at(20.0)).complete, "2 s late is tolerated");
        assert!(!log.slice(at(10.0), at(30.0)).complete, "the last 10 s are unknown");
        assert!(!RollingLog::default().slice(at(0.0), at(1.0)).complete);
        assert!(RollingLog::default().slice(at(0.0), at(1.0)).snapshots.is_empty());
    }

    #[test]
    fn failures_store_nothing() {
        let mut log = RollingLog::default();
        log.record(at(0.0), None);
        assert_eq!(log.slice(at(0.0), at(5.0)), Slice::default());
    }

    #[test]
    fn the_thread_samples_on_its_period_stops_on_drop_and_a_save_looks_once_more() {
        let looks = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&looks);
        let ring = WindowRing::start_with(
            move || {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                Some(vec![win(if n < 3 { 1 } else { 2 })])
            },
            Duration::from_millis(20),
        );
        let began = SystemTime::now();
        thread::sleep(Duration::from_millis(400));
        let slice = ring.finish(began, SystemTime::now());
        let mut ids: Vec<u32> = slice.snapshots.iter().map(|s| s.windows[0].id).collect();
        ids.dedup();
        assert_eq!(ids, [1, 2], "{:?}", lists(&slice));
        assert!(slice.complete);
        drop(ring);
        thread::sleep(Duration::from_millis(100));
        let stopped = looks.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(looks.load(Ordering::SeqCst), stopped, "no more looks after the ring is dropped");
        assert!(stopped >= 5);
    }

    #[test]
    fn a_ring_whose_looks_all_fail_has_nothing_and_says_so() {
        let ring = WindowRing::start_with(|| None, Duration::from_millis(20));
        thread::sleep(Duration::from_millis(60));
        let slice = ring.finish(SystemTime::now() - Duration::from_secs(5), SystemTime::now());
        assert_eq!(slice, Slice::default());
    }
}
