//! The ink overlay: a `wlr-layer-shell` surface over one output that shows what ⇧ draws, and
//! is in the video since `wf-recorder` records the composited output. It has its own Wayland
//! connection and thread, no input region (every click and hover goes to what is underneath),
//! and maps nothing until a capture has ink to show.
//!
//! GPUI 0.2 can't make a layer-shell surface, and a pinned transparent GPUI window has no
//! fixed place in the stacking, no exact colour, and needs a window rule per output.

pub mod raster;
pub mod scene;
mod surface;

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use super::SharedCaptureTelemetry;
use crate::capture::scrawl_log;
use crate::screens::{Rect, Screen};

/// How long `open` waits for the compositor to configure the surface.
const OPEN_TIMEOUT: Duration = Duration::from_secs(4);

/// One output as the compositor describes it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Output {
    /// Its place in the global points (the logical rect), `[x, y, w, h]`.
    pub rect: Rect,
    /// The integer scale the compositor reports for it.
    pub scale: i32,
}

/// The output for a rect `on` (global points): the one holding its centre, else the one it
/// overlaps most, else the one under the pointer, else the first; the rules of
/// `screens::index_for_rect`. `None` only without outputs.
pub fn pick_output(
    outputs: &[Output],
    on: Option<Rect>,
    pointer: Option<[f64; 2]>,
) -> Option<usize> {
    let screens: Vec<Screen> = outputs
        .iter()
        .enumerate()
        .map(|(i, o)| Screen {
            id: i as u32,
            frame: o.rect,
            visible: o.rect,
            scale: f64::from(o.scale),
            safe_top: 0.0,
        })
        .collect();
    on.and_then(|r| crate::screens::index_for_rect(&screens, r))
        .or_else(|| pointer.and_then(|p| crate::screens::index_at(&screens, p)))
        .or(if screens.is_empty() { None } else { Some(0) })
}

/// The buffer scale to draw an output at: the integer the compositor reports, unless the
/// monitor is scaled by a fraction (Hyprland's `scale` is 1.25, say, while it reports 2 to
/// clients). Fractional scaling needs `wp_fractional_scale` and a viewport, which this doesn't
/// do, so it draws at 1, which the compositor stretches: the ink is in the right place, a
/// little soft. Returns the scale, and what to log when it fell back.
pub fn buffer_scale(compositor_scale: i32, monitor_scale: Option<f64>) -> (i32, Option<String>) {
    match monitor_scale {
        Some(scale) if scale.is_finite() && (scale - scale.round()).abs() > 0.01 => (
            1,
            Some(format!(
                "the output is scaled by {scale}, which the ink overlay can't draw at \
                 (no fractional scale yet): drawing at 1, so it is soft but in place"
            )),
        ),
        _ => (compositor_scale.max(1), None),
    }
}

fn ring(fd: &OwnedFd) {
    let one = 1_u64;
    unsafe {
        libc::write(fd.as_raw_fd(), (&raw const one).cast(), 8);
    }
}

/// What the overlay's thread and its handle share.
struct Shared {
    stop: AtomicBool,
    /// A request to move to the output of this rect, until the thread takes it.
    move_to: Mutex<Option<Rect>>,
    /// The outputs the compositor lists, kept current by the thread.
    outputs: Mutex<Vec<Output>>,
}

/// The ink overlay, open over one output. Dropping it removes the surface and ends its thread.
/// (Named for the macOS Metal overlay the shared UI code opens.)
pub struct MetalLaserOverlay {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    telemetry: Arc<SharedCaptureTelemetry>,
    wake: Arc<OwnedFd>,
    /// The output it covers, global points.
    screen: Rect,
}

impl MetalLaserOverlay {
    /// Opens over the output holding the centre of `on` (global points, top-left origin, like
    /// capture targets), else the output under the pointer. Returns once the compositor has
    /// configured the surface; an error says why there is no ink.
    pub fn open(telemetry: Arc<SharedCaptureTelemetry>, on: Option<Rect>) -> Result<Self, String> {
        let wake = telemetry
            .ink_wake
            .attach()
            .map_err(|error| format!("could not make the overlay's wake-up fd: {error}"))?;
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            move_to: Mutex::new(None),
            outputs: Mutex::new(Vec::new()),
        });
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let launch = surface::Launch {
            telemetry: Arc::clone(&telemetry),
            shared: Arc::clone(&shared),
            wake: Arc::clone(&wake),
            on,
            pointer: crate::screens::pointer(),
            scale_of: monitor_scales(),
        };
        let thread = std::thread::Builder::new()
            .name("scrawl-ink-overlay".into())
            .spawn(move || surface::run(launch, ready_tx))
            .map_err(|error| format!("could not start the overlay's thread: {error}"));
        let outcome = thread.and_then(|thread| match ready_rx.recv_timeout(OPEN_TIMEOUT) {
            Ok(Ok(screen)) => Ok((thread, screen)),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                shared.stop.store(true, Ordering::Release);
                ring(&wake);
                let _ = thread.join();
                Err("the compositor never configured the ink surface".to_string())
            }
        });
        match outcome {
            Ok((thread, screen)) => Ok(Self {
                shared,
                thread: Some(thread),
                telemetry,
                wake,
                screen,
            }),
            Err(error) => {
                telemetry.ink_wake.detach(&wake);
                Err(error)
            }
        }
    }

    /// Nothing excludes a window from a Linux recording (it is the composited output), so
    /// there is no window to name.
    pub fn window_number(&self) -> u32 {
        0
    }

    /// The output it covers, global points.
    pub fn screen(&self) -> Rect {
        self.screen
    }

    /// Moves to the output `rect` (global points) belongs to. Returns whether it moved.
    pub fn move_to(&mut self, rect: Rect) -> bool {
        let outputs = self
            .shared
            .outputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(index) = pick_output(&outputs, Some(rect), None) else {
            return false;
        };
        let target = outputs[index].rect;
        if (0..4).all(|i| (target[i] - self.screen[i]).abs() < 1.0) {
            return false;
        }
        *self
            .shared
            .move_to
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(target);
        ring(&self.wake);
        self.screen = target;
        true
    }
}

impl Drop for MetalLaserOverlay {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        ring(&self.wake);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            scrawl_log("The ink overlay's thread panicked");
        }
        self.telemetry.ink_wake.detach(&self.wake);
    }
}

/// Each Hyprland monitor's rect and scale, for `buffer_scale`'s fractional check (the
/// compositor only reports an integer).
fn monitor_scales() -> Vec<(Rect, f64)> {
    crate::screens::displays()
        .into_iter()
        .map(|s| (s.frame, s.scale))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(rect: Rect) -> Output {
        Output { rect, scale: 1 }
    }

    const LEFT: Rect = [0.0, 0.0, 1920.0, 1080.0];
    const RIGHT: Rect = [1920.0, 0.0, 2560.0, 1440.0];

    #[test]
    fn the_output_is_the_one_holding_the_targets_centre() {
        let outputs = [output(LEFT), output(RIGHT)];
        // A window mostly on the right, its corner over the seam.
        assert_eq!(
            pick_output(&outputs, Some([1800.0, 100.0, 1000.0, 500.0]), None),
            Some(1)
        );
        assert_eq!(
            pick_output(&outputs, Some([100.0, 100.0, 500.0, 500.0]), Some([2000.0, 10.0])),
            Some(0)
        );
    }

    #[test]
    fn with_no_target_the_pointers_output_is_used_else_the_first() {
        let outputs = [output(LEFT), output(RIGHT)];
        assert_eq!(pick_output(&outputs, None, Some([2500.0, 700.0])), Some(1));
        // A pointer in a gap between outputs goes to the nearest.
        assert_eq!(pick_output(&outputs, None, Some([100.0, 1300.0])), Some(0));
        // Nothing known: the first.
        assert_eq!(pick_output(&outputs, None, None), Some(0));
        assert_eq!(pick_output(&[], Some(LEFT), Some([1.0, 1.0])), None);
    }

    #[test]
    fn a_target_off_every_output_is_given_the_nearest() {
        let outputs = [output(LEFT), output(RIGHT)];
        let far_right = Some([9000.0, 100.0, 100.0, 100.0]);
        assert_eq!(pick_output(&outputs, far_right, Some([10.0, 10.0])), Some(1));
    }

    #[test]
    fn a_target_straddling_the_seam_goes_where_most_of_it_is_when_its_centre_is_in_a_gap() {
        // Two outputs of different height: the centre of this rect sits below the left one.
        let outputs = [output([0.0, 0.0, 1920.0, 1080.0]), output([1920.0, 0.0, 1280.0, 2000.0])];
        assert_eq!(
            pick_output(&outputs, Some([1500.0, 1500.0, 1000.0, 500.0]), None),
            Some(1)
        );
    }

    #[test]
    fn the_buffer_scale_is_the_compositors_unless_the_monitor_is_scaled_by_a_fraction() {
        assert_eq!(buffer_scale(1, Some(1.0)), (1, None));
        assert_eq!(buffer_scale(2, Some(2.0)), (2, None));
        assert_eq!(buffer_scale(2, None), (2, None));
        assert_eq!(buffer_scale(0, None), (1, None));
        // Hyprland's 1.25 reaches clients as 2.
        let (scale, note) = buffer_scale(2, Some(1.25));
        assert_eq!(scale, 1);
        assert!(note.unwrap().contains("1.25"));
        assert_eq!(buffer_scale(2, Some(1.5)).0, 1);
        // Float noise on a whole scale isn't a fraction.
        assert_eq!(buffer_scale(2, Some(2.000_000_1)), (2, None));
    }
}
