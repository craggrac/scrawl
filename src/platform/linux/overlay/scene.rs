//! What the ink overlay shows at one moment, as plain shapes in the output's points. This is
//! the stroke-to-segments loop of `laser_overlay.rs::draw` (macOS), copied rather than shared
//! because there it is entangled with the Metal buffer structs: keep the constants and the
//! fade in step with it. Laser strokes are not drawn (nothing on macOS makes one either).

use crate::model::Stroke;
use crate::sketch::smooth_ink_points;

/// Segments drawn at most; past it the oldest go, so the tip being drawn keeps showing.
pub const MAX_SEGMENTS: usize = 16_384;
/// Pins drawn at most.
pub const MAX_PINS: usize = 64;
/// A pin's radius in points (its halo adds more).
pub const PIN_RADIUS_PT: f32 = 11.5;
/// Finished ink stays at full opacity this long after its last point, then fades over as long.
pub const HOLD_MS: u64 = 650;
pub const FADE_MS: u64 = 650;

/// One piece of ink: a line from `a` to `b` whose width follows `w0` to `w1` (the simulated
/// pen pressure), at `alpha` of full opacity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Segment {
    pub a: [f32; 2],
    pub b: [f32; 2],
    pub w0: f32,
    pub w1: f32,
    pub alpha: f32,
}

/// A numbered pin's marker (the number is only on the keyframes).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pin {
    pub pos: [f32; 2],
    pub radius: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scene {
    pub segments: Vec<Segment>,
    pub pins: Vec<Pin>,
    /// The dot at the pointer while ⇧ is held.
    pub tip: Option<[f32; 2]>,
}

impl Scene {
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty() && self.pins.is_empty() && self.tip.is_none()
    }
}

/// Where a stroke's `0..=1` points land: `target` is the recorded rect (`x, y, w, h`) in the
/// output's own points.
fn to_output(target: [f32; 4], [nx, ny]: [f32; 2]) -> [f32; 2] {
    [target[0] + nx * target[2], target[1] + ny * target[3]]
}

/// The opacity of finished ink `age_ms` after its last point: full while it holds, then
/// `(1 - t)²` as it fades; `None` once it is gone.
pub fn finished_alpha(age_ms: u64) -> Option<f32> {
    let gone = HOLD_MS + FADE_MS;
    if age_ms >= gone {
        None
    } else if age_ms <= HOLD_MS {
        Some(1.0)
    } else {
        let t = ((age_ms - HOLD_MS) as f32 / FADE_MS as f32).clamp(0.0, 1.0);
        Some((1.0 - t) * (1.0 - t))
    }
}

/// The scene at `now_ms` (the capture clock) for `strokes` (finished, oldest first) and the
/// stroke being drawn. `target` is the recorded rect in output points; `tip` is the pointer
/// in output points while ⇧ is held and the capture runs.
pub fn build(
    strokes: &[Stroke],
    live: Option<&Stroke>,
    now_ms: u64,
    target: [f32; 4],
    tip: Option<[f32; 2]>,
) -> Scene {
    let mut segments = Vec::with_capacity(256);
    let mut pins = Vec::new();
    for (stroke, is_live) in strokes
        .iter()
        .map(|s| (s, false))
        .chain(live.map(|s| (s, true)))
    {
        if stroke.pin.is_some() {
            if let Some(&first) = stroke.normalized_points().first()
                && pins.len() < MAX_PINS
            {
                pins.push(Pin {
                    pos: to_output(target, first),
                    radius: PIN_RADIUS_PT,
                });
            }
            continue;
        }
        if stroke.laser {
            continue;
        }
        // Live ink stays at 100% while ⇧ is held, then holds and fades once it is finished.
        let alpha = if is_live {
            1.0
        } else {
            match finished_alpha(now_ms.saturating_sub(stroke.t1)) {
                Some(alpha) => alpha,
                None => continue,
            }
        };
        let points: Vec<[f32; 2]> = stroke
            .normalized_points()
            .into_iter()
            .map(|p| to_output(target, p))
            .collect();
        for pair in smooth_ink_points(&points).windows(2) {
            segments.push(Segment {
                a: pair[0].0,
                b: pair[1].0,
                w0: pair[0].1,
                w1: pair[1].1,
                alpha,
            });
        }
    }
    if segments.len() > MAX_SEGMENTS {
        segments.drain(..segments.len() - MAX_SEGMENTS);
    }
    Scene {
        segments,
        pins,
        tip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SCALE;

    /// A stroke of `points` (0..=1) at `t0`, one point every 10 ms.
    fn stroke(t0: u64, points: &[[f32; 2]]) -> Stroke {
        Stroke {
            t0,
            t1: t0 + 10 * (points.len() as u64 - 1),
            fw: 1000,
            fh: 500,
            on: None,
            pin: None,
            laser: false,
            pts: points
                .iter()
                .enumerate()
                .flat_map(|(i, p)| {
                    [
                        (p[0] * SCALE as f32).round() as i32,
                        (p[1] * SCALE as f32).round() as i32,
                        10 * i as i32,
                    ]
                })
                .collect(),
        }
    }

    const LINE: [[f32; 2]; 4] = [[0.1, 0.5], [0.3, 0.5], [0.5, 0.5], [0.7, 0.5]];
    const TARGET: [f32; 4] = [0.0, 0.0, 1000.0, 500.0];

    fn alphas(scene: &Scene) -> Vec<f32> {
        scene.segments.iter().map(|s| s.alpha).collect()
    }

    #[test]
    fn finished_ink_holds_for_650_ms_then_fades_squared_over_650_and_is_dropped() {
        let ink = [stroke(1_000, &LINE)];
        let end = ink[0].t1;
        let at = |age: u64| build(&ink, None, end + age, TARGET, None);
        assert!(at(0).segments.iter().all(|s| s.alpha == 1.0));
        assert!(!at(0).segments.is_empty());
        assert!(at(650).segments.iter().all(|s| s.alpha == 1.0));
        // Halfway through the fade: (1 - 0.5)².
        assert!(at(975).segments.iter().all(|s| (s.alpha - 0.25).abs() < 1e-6));
        // Three quarters of the way (487 of 650 ms): (1 - 0.749)².
        assert!(at(1_137).segments.iter().all(|s| (s.alpha - 0.0629).abs() < 1e-3));
        assert!(at(1_299).segments.iter().all(|s| s.alpha < 1e-4));
        assert!(at(1_300).is_empty());
        assert!(at(60_000).is_empty());
    }

    #[test]
    fn the_stroke_being_drawn_is_always_at_full_opacity() {
        let live = stroke(1_000, &LINE);
        let scene = build(&[], Some(&live), 90_000, TARGET, None);
        assert!(!scene.segments.is_empty());
        assert_eq!(alphas(&scene), vec![1.0; scene.segments.len()]);
    }

    #[test]
    fn points_land_in_the_recorded_rect_of_the_output() {
        let live = stroke(0, &LINE);
        // The target is a 500x250 window at (300, 120) on this output.
        let scene = build(&[], Some(&live), 0, [300.0, 120.0, 500.0, 250.0], None);
        let first = scene.segments.first().unwrap();
        let last = scene.segments.last().unwrap();
        // (0.1, 0.5) and (0.7, 0.5) of the window.
        assert_eq!(first.a, [350.0, 245.0]);
        assert_eq!(last.b, [650.0, 245.0]);
    }

    #[test]
    fn a_pin_is_a_marker_at_its_point_that_never_fades() {
        let pin = Stroke {
            pin: Some(1),
            ..stroke(1_000, &[[0.25, 0.4]])
        };
        for now in [1_000, 3_000, 600_000] {
            let scene = build(std::slice::from_ref(&pin), None, now, [100.0, 50.0, 800.0, 400.0], None);
            assert!(scene.segments.is_empty());
            assert_eq!(
                scene.pins,
                vec![Pin {
                    pos: [300.0, 210.0],
                    radius: 11.5
                }]
            );
        }
    }

    #[test]
    fn the_tip_dot_is_the_pointer_and_only_when_given() {
        let tip = [321.5, 40.0];
        assert_eq!(build(&[], None, 0, TARGET, Some(tip)).tip, Some(tip));
        assert_eq!(build(&[], None, 0, TARGET, None).tip, None);
        assert!(build(&[], None, 0, TARGET, None).is_empty());
        assert!(!build(&[], None, 0, TARGET, Some(tip)).is_empty());
    }

    #[test]
    fn laser_strokes_are_not_drawn() {
        let laser = Stroke {
            laser: true,
            ..stroke(1_000, &LINE)
        };
        let scene = build(&[laser.clone()], Some(&laser), 1_000, TARGET, None);
        assert!(scene.is_empty());
    }

    #[test]
    fn a_single_point_stroke_has_no_segment_and_no_pin() {
        let dot = stroke(0, &[[0.5, 0.5]]);
        assert!(build(&[dot], None, 0, TARGET, None).is_empty());
    }

    #[test]
    fn past_the_segment_cap_the_oldest_go_and_the_live_tip_stays() {
        // 60 strokes of ~300 segments each is well past the cap.
        let zig: Vec<[f32; 2]> = (0..400)
            .map(|i| [0.1 + 0.8 * (i % 2) as f32, 0.1 + 0.8 * (i as f32 / 400.0)])
            .collect();
        // All still holding at full opacity: each ends within 650 ms of `now`.
        let old: Vec<Stroke> = (0..60).map(|i| stroke(i * 10, &zig)).collect();
        let live = stroke(4_600, &LINE);
        let scene = build(&old, Some(&live), 4_650, TARGET, None);
        assert_eq!(scene.segments.len(), MAX_SEGMENTS);
        // The live stroke, the newest, is whole at the end.
        let tail = build(&[], Some(&live), 4_650, TARGET, None).segments;
        assert_eq!(&scene.segments[MAX_SEGMENTS - tail.len()..], &tail[..]);
    }

    #[test]
    fn at_most_sixty_four_pins() {
        let pins: Vec<Stroke> = (0..70)
            .map(|i| Stroke {
                pin: Some(i + 1),
                ..stroke(u64::from(i), &[[0.5, 0.5]])
            })
            .collect();
        assert_eq!(build(&pins, None, 0, TARGET, None).pins.len(), 64);
    }
}
