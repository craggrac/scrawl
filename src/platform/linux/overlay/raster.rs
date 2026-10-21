//! Draws a `Scene` into a premultiplied BGRA canvas (`wl_shm`'s `Argb8888` on little-endian),
//! touching only what changed. This is the ink, pin and tip-dot branches of
//! `laser_overlay.metal`, per pixel, with the same constants and the same final premultiply:
//! each pixel's coverage is the maximum over the shapes that reach it (not a sum), a pin has a
//! white halo under its red fill, and the ink is `#ff3b30`.

use super::scene::{Pin, Scene, Segment};

/// The ink's red, as the shader's `ink_col`.
const INK: [f32; 3] = [1.0, 0.23, 0.19];
/// The halo's opacity around a pin.
const HALO_ALPHA: f32 = 0.82;
/// Ink radius in points at a pressure of 1; the pressure is clamped to this range.
const BASE_RADIUS_PT: f32 = 2.25;
const PRESSURE_RANGE: (f32, f32) = (0.55, 1.15);
/// The shader drops a segment past this many points beyond its radius.
const CUTOFF_PT: f32 = 2.0;
/// How far a segment can colour a pixel from its line.
const INK_REACH_PT: f32 = BASE_RADIUS_PT * PRESSURE_RANGE.1 + CUTOFF_PT;
/// How far a pin's halo reaches from its centre beyond the radius.
const PIN_REACH_PT: f32 = 3.5;
/// The tip dot.
const TIP_REACH_PT: f32 = 5.0;

/// A box of pixels, `x0..x1` by `y0..y1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl Rect {
    fn union(self, other: Self) -> Self {
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    pub fn width(self) -> i32 {
        self.x1 - self.x0
    }

    pub fn height(self) -> i32 {
        self.y1 - self.y0
    }
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// What a pixel gets from the scene: the ink's coverage and the halo's.
#[derive(Clone, Copy, Default)]
struct Coverage {
    red: f32,
    halo: f32,
}

/// The canvas, in device pixels, at an integer buffer `scale` of the scene's points.
pub struct Raster {
    width: i32,
    height: i32,
    scale: f32,
    bgra: Vec<u8>,
    /// What the last `draw` painted: the next one clears it.
    painted: Option<Rect>,
    /// Scratch coverage for the box being drawn.
    cover: Vec<Coverage>,
}

impl Raster {
    pub fn new(width: u32, height: u32, scale: u32) -> Self {
        let (width, height) = (width as i32, height as i32);
        Self {
            width,
            height,
            scale: scale.max(1) as f32,
            bgra: vec![0; width as usize * height as usize * 4],
            painted: None,
            cover: Vec::new(),
        }
    }

    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    /// The whole canvas, `width * 4` bytes a row, bytes B, G, R, A.
    pub fn pixels(&self) -> &[u8] {
        &self.bgra
    }

    /// One pixel's bytes: B, G, R, A.
    pub fn pixel(&self, x: i32, y: i32) -> [u8; 4] {
        let at = (y as usize * self.width as usize + x as usize) * 4;
        [
            self.bgra[at],
            self.bgra[at + 1],
            self.bgra[at + 2],
            self.bgra[at + 3],
        ]
    }

    /// Draws `scene`. Returns the box of pixels that may differ from before (what was
    /// painted last time and what is now), or `None` if nothing was or is painted.
    pub fn draw(&mut self, scene: &Scene) -> Option<Rect> {
        let now = self.bounds(scene);
        let damage = match (self.painted, now) {
            (Some(before), Some(now)) => before.union(now),
            (Some(only), None) | (None, Some(only)) => only,
            (None, None) => return None,
        };
        // What was painted is cleared, then the scene goes on top.
        self.fill_clear(damage);
        if let Some(now) = now {
            self.paint(scene, now);
        }
        self.painted = now;
        Some(damage)
    }

    /// The pixels the scene can reach, clipped to the canvas.
    fn bounds(&self, scene: &Scene) -> Option<Rect> {
        let mut bounds: Option<Rect> = None;
        let mut grow = |lo: [f32; 2], hi: [f32; 2], reach: f32| {
            let scale = self.scale;
            let rect = Rect {
                x0: (((lo[0] - reach) * scale).floor() as i32).max(0),
                y0: (((lo[1] - reach) * scale).floor() as i32).max(0),
                x1: (((hi[0] + reach) * scale).ceil() as i32 + 1).min(self.width),
                y1: (((hi[1] + reach) * scale).ceil() as i32 + 1).min(self.height),
            };
            if rect.x0 < rect.x1 && rect.y0 < rect.y1 {
                bounds = Some(bounds.map_or(rect, |b| b.union(rect)));
            }
        };
        for s in &scene.segments {
            grow(
                [s.a[0].min(s.b[0]), s.a[1].min(s.b[1])],
                [s.a[0].max(s.b[0]), s.a[1].max(s.b[1])],
                INK_REACH_PT,
            );
        }
        for p in &scene.pins {
            grow(p.pos, p.pos, p.radius + PIN_REACH_PT);
        }
        if let Some(tip) = scene.tip {
            grow(tip, tip, TIP_REACH_PT);
        }
        bounds
    }

    fn fill_clear(&mut self, rect: Rect) {
        let row = rect.width() as usize * 4;
        for y in rect.y0..rect.y1 {
            let at = (y as usize * self.width as usize + rect.x0 as usize) * 4;
            self.bgra[at..at + row].fill(0);
        }
    }

    fn paint(&mut self, scene: &Scene, area: Rect) {
        let (w, h) = (area.width() as usize, area.height() as usize);
        self.cover.clear();
        self.cover.resize(w * h, Coverage::default());
        for segment in &scene.segments {
            self.ink(segment, area);
        }
        for pin in &scene.pins {
            self.pin(pin, area);
        }
        if let Some(tip) = scene.tip {
            self.tip(tip, area);
        }
        for y in 0..h {
            for x in 0..w {
                let cov = self.cover[y * w + x];
                if cov.red <= 0.001 && cov.halo <= 0.001 {
                    continue;
                }
                let at = ((area.y0 as usize + y) * self.width as usize + area.x0 as usize + x) * 4;
                self.bgra[at..at + 4].copy_from_slice(&composite(cov));
            }
        }
    }

    /// The pixels of `shape_box` (points, grown by `reach`) that are inside `area`.
    fn pixel_box(&self, lo: [f32; 2], hi: [f32; 2], reach: f32, area: Rect) -> Rect {
        let scale = self.scale;
        Rect {
            x0: (((lo[0] - reach) * scale).floor() as i32).max(area.x0),
            y0: (((lo[1] - reach) * scale).floor() as i32).max(area.y0),
            x1: ((((hi[0] + reach) * scale).ceil() as i32) + 1).min(area.x1),
            y1: ((((hi[1] + reach) * scale).ceil() as i32) + 1).min(area.y1),
        }
    }

    fn ink(&mut self, s: &Segment, area: Rect) {
        let boxed = self.pixel_box(
            [s.a[0].min(s.b[0]), s.a[1].min(s.b[1])],
            [s.a[0].max(s.b[0]), s.a[1].max(s.b[1])],
            INK_REACH_PT,
            area,
        );
        let ab = [s.b[0] - s.a[0], s.b[1] - s.a[1]];
        let len2 = (ab[0] * ab[0] + ab[1] * ab[1]).max(0.0001);
        let alpha = s.alpha.clamp(0.0, 1.0);
        let stride = area.width() as usize;
        for y in boxed.y0..boxed.y1 {
            for x in boxed.x0..boxed.x1 {
                let p = [(x as f32 + 0.5) / self.scale, (y as f32 + 0.5) / self.scale];
                let h = (((p[0] - s.a[0]) * ab[0] + (p[1] - s.a[1]) * ab[1]) / len2).clamp(0.0, 1.0);
                let d = (p[0] - (s.a[0] + ab[0] * h)).hypot(p[1] - (s.a[1] + ab[1] * h));
                let w = (s.w0 + (s.w1 - s.w0) * h).clamp(0.0, 1.25);
                let r = BASE_RADIUS_PT * w.clamp(PRESSURE_RANGE.0, PRESSURE_RANGE.1);
                if d < r + CUTOFF_PT {
                    let fill = smoothstep(r + 0.70, (r - 0.70).max(0.0), d) * alpha;
                    let slot = &mut self.cover
                        [(y - area.y0) as usize * stride + (x - area.x0) as usize];
                    slot.red = slot.red.max(fill);
                }
            }
        }
    }

    fn pin(&mut self, pin: &Pin, area: Rect) {
        let reach = pin.radius + PIN_REACH_PT;
        let boxed = self.pixel_box(pin.pos, pin.pos, reach, area);
        let stride = area.width() as usize;
        for y in boxed.y0..boxed.y1 {
            for x in boxed.x0..boxed.x1 {
                let p = [(x as f32 + 0.5) / self.scale, (y as f32 + 0.5) / self.scale];
                let d = (p[0] - pin.pos[0]).hypot(p[1] - pin.pos[1]);
                if d < reach {
                    let halo = smoothstep(pin.radius + 2.0, pin.radius + 0.8, d);
                    let fill = smoothstep(pin.radius + 0.75, pin.radius - 0.75, d);
                    let slot = &mut self.cover
                        [(y - area.y0) as usize * stride + (x - area.x0) as usize];
                    slot.halo = slot.halo.max(halo);
                    slot.red = slot.red.max(fill);
                }
            }
        }
    }

    fn tip(&mut self, tip: [f32; 2], area: Rect) {
        let boxed = self.pixel_box(tip, tip, TIP_REACH_PT, area);
        let stride = area.width() as usize;
        for y in boxed.y0..boxed.y1 {
            for x in boxed.x0..boxed.x1 {
                let p = [(x as f32 + 0.5) / self.scale, (y as f32 + 0.5) / self.scale];
                let d = (p[0] - tip[0]).hypot(p[1] - tip[1]);
                if d < TIP_REACH_PT {
                    let fill = smoothstep(2.6, 1.4, d);
                    let slot = &mut self.cover
                        [(y - area.y0) as usize * stride + (x - area.x0) as usize];
                    slot.red = slot.red.max(fill);
                }
            }
        }
    }
}

/// The shader's compositing of the halo, then the ink, then its final premultiply, as
/// B, G, R, A bytes.
fn composite(cov: Coverage) -> [u8; 4] {
    let mut rgb = [0.0_f32; 3];
    let mut a = 0.0_f32;
    if cov.halo > 0.001 {
        let h = cov.halo * HALO_ALPHA;
        rgb = [h; 3];
        a = h;
    }
    if cov.red > 0.001 {
        let r = cov.red;
        for (out, ink) in rgb.iter_mut().zip(INK) {
            *out = ink * r + *out * (1.0 - r);
        }
        a = r + a * (1.0 - r);
    }
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [byte(rgb[2] * a), byte(rgb[1] * a), byte(rgb[0] * a), byte(a)]
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED_FULL: [u8; 4] = [48, 59, 255, 255];

    fn seg(a: [f32; 2], b: [f32; 2], w: f32, alpha: f32) -> Segment {
        Segment {
            a,
            b,
            w0: w,
            w1: w,
            alpha,
        }
    }

    fn scene(segments: Vec<Segment>) -> Scene {
        Scene {
            segments,
            ..Scene::default()
        }
    }

    /// A horizontal line along the centre of pixel row 100.
    fn line(w: f32, alpha: f32) -> Scene {
        scene(vec![seg([100.0, 100.5], [200.0, 100.5], w, alpha)])
    }

    #[test]
    fn the_centre_of_a_segment_is_the_inks_exact_premultiplied_red() {
        let mut raster = Raster::new(400, 300, 1);
        raster.draw(&line(1.0, 1.0));
        assert_eq!(raster.pixel(150, 100), RED_FULL);
        // Outside it, nothing.
        assert_eq!(raster.pixel(150, 90), [0; 4]);
        assert_eq!(raster.pixel(50, 100), [0; 4]);
    }

    #[test]
    fn the_edge_is_antialiased_and_ends_past_the_radius_plus_seven_tenths() {
        let mut raster = Raster::new(400, 300, 1);
        raster.draw(&line(1.0, 1.0));
        // Radius 2.25. Row 102's centre is 2 away: inside the soft edge (0.757 coverage).
        let soft = raster.pixel(150, 102);
        assert!(soft[3] > 150 && soft[3] < 230, "{soft:?}");
        // Row 103's is 3 away, past 2.25 + 0.7.
        assert_eq!(raster.pixel(150, 103), [0; 4]);
        assert_eq!(raster.pixel(150, 97), [0; 4]);
        // Row 101 and 99 are 1 away: whole.
        assert_eq!(raster.pixel(150, 101)[3], 255);
        assert_eq!(raster.pixel(150, 99)[3], 255);
    }

    #[test]
    fn the_width_follows_the_pressure_between_its_clamps() {
        let alpha_at = |w: f32, row: i32| {
            let mut raster = Raster::new(400, 300, 1);
            raster.draw(&line(w, 1.0));
            raster.pixel(150, row)[3]
        };
        // 0.55 -> radius 1.24: two rows out (d = 2) is nothing; 1.15 -> 2.59: nearly whole.
        assert_eq!(alpha_at(0.55, 102), 0);
        assert!(alpha_at(1.15, 102) > 240, "{}", alpha_at(1.15, 102));
        // Below the range is the range's floor, above it its ceiling.
        assert_eq!(alpha_at(0.1, 101), alpha_at(0.55, 101));
        assert_eq!(alpha_at(1.25, 102), alpha_at(1.15, 102));
        // A pressure that changes along the segment (0.55 -> 1.15) is thin, then thick.
        let mut raster = Raster::new(400, 300, 1);
        raster.draw(&scene(vec![Segment {
            a: [100.0, 100.5],
            b: [200.0, 100.5],
            w0: 0.55,
            w1: 1.15,
            alpha: 1.0,
        }]));
        assert_eq!(raster.pixel(102, 102)[3], 0);
        assert!(raster.pixel(198, 102)[3] > 240);
    }

    #[test]
    fn faded_ink_is_premultiplied_once_more_as_the_shader_does() {
        let mut raster = Raster::new(400, 300, 1);
        raster.draw(&line(1.0, 0.5));
        // alpha 0.5; colour 0.5 * 0.5 of the ink.
        assert_eq!(raster.pixel(150, 100), [12, 15, 64, 128]);
    }

    #[test]
    fn overlapping_ink_keeps_the_larger_coverage_not_the_sum() {
        let mut raster = Raster::new(400, 300, 1);
        raster.draw(&scene(vec![
            seg([100.0, 100.5], [200.0, 100.5], 1.0, 0.5),
            seg([150.0, 100.5], [250.0, 100.5], 1.0, 0.5),
            seg([140.0, 100.5], [160.0, 100.5], 1.0, 0.3),
        ]));
        // Three layers over (150, 100): the max, 0.5, and not 1 - 0.5 * 0.5 * 0.7.
        assert_eq!(raster.pixel(150, 100)[3], 128);
        assert_eq!(raster.pixel(120, 100)[3], 128);
    }

    #[test]
    fn a_pin_is_a_red_disc_of_11_and_a_half_points_in_a_white_ring() {
        let mut raster = Raster::new(300, 300, 1);
        raster.draw(&Scene {
            pins: vec![Pin {
                pos: [150.0, 150.5],
                radius: 11.5,
            }],
            ..Scene::default()
        });
        assert_eq!(raster.pixel(150, 150), RED_FULL);
        // 10.5 out is whole red; the fill's soft edge runs from 10.75 to 12.25.
        assert_eq!(raster.pixel(160, 150), RED_FULL);
        let edge = raster.pixel(161, 150);
        assert!(edge[3] > 200 && edge != RED_FULL, "{edge:?}");
        // 12.5 out: past the fill, in the halo: white, at most 0.82 opaque.
        let halo = raster.pixel(162, 150);
        assert!(halo[3] > 150 && halo[3] <= 209, "{halo:?}");
        assert_eq!((halo[0], halo[1]), (halo[2], halo[2]), "white, not red: {halo:?}");
        // Beyond the halo (radius + 3.5), nothing.
        assert_eq!(raster.pixel(165, 150), [0; 4]);
        assert_eq!(raster.pixel(150, 166), [0; 4]);
    }

    #[test]
    fn the_tip_dot_is_a_small_red_dot_at_the_pointer() {
        let mut raster = Raster::new(300, 300, 1);
        raster.draw(&Scene {
            tip: Some([100.5, 100.5]),
            ..Scene::default()
        });
        assert_eq!(raster.pixel(100, 100), RED_FULL);
        assert_eq!(raster.pixel(101, 100), RED_FULL);
        // Radius 2.6 soft to 1.4: 3 px out is nothing.
        assert_eq!(raster.pixel(103, 100), [0; 4]);
        assert_eq!(raster.pixel(100, 104), [0; 4]);
    }

    #[test]
    fn at_scale_two_the_ink_is_the_same_size_in_points_and_twice_the_pixels() {
        let mut raster = Raster::new(800, 600, 2);
        raster.draw(&scene(vec![seg([100.0, 100.25], [200.0, 100.25], 1.0, 1.0)]));
        // Row 200 (device) is y 100.25 pt: the centre. Radius 2.25 pt = 4.5 px.
        assert_eq!(raster.pixel(300, 200), RED_FULL);
        assert_eq!(raster.pixel(300, 203)[3], 255);
        assert_eq!(raster.pixel(300, 206), [0; 4]);
        assert_eq!(raster.pixel(300, 194), [0; 4]);
        // Its end is where the point is (100 pt), doubled.
        assert_eq!(raster.pixel(201, 200)[3], 255);
        assert_eq!(raster.pixel(190, 200), [0; 4]);
    }

    #[test]
    fn the_damage_is_what_changed_and_an_empty_scene_clears_only_what_was_painted() {
        let mut raster = Raster::new(400, 300, 1);
        assert_eq!(raster.draw(&Scene::default()), None);
        let first = raster.draw(&line(1.0, 1.0)).unwrap();
        // The box of the segment and its reach (4.5875 -> to the pixel).
        assert!(first.x0 <= 95 && first.x1 >= 205 && first.y0 <= 96 && first.y1 >= 105);
        assert!(first.x0 >= 90 && first.x1 <= 210 && first.y0 >= 90 && first.y1 <= 110);

        // Another stroke elsewhere: the damage is both, since the first is wiped and redrawn.
        let second = raster
            .draw(&scene(vec![seg([300.0, 200.5], [350.0, 200.5], 1.0, 1.0)]))
            .unwrap();
        assert!(second.x0 <= first.x0 && second.x1 >= 350 && second.y1 >= 205);
        assert_eq!(raster.pixel(150, 100), [0; 4], "the first is gone");
        assert_eq!(raster.pixel(320, 200), RED_FULL);

        // An empty scene wipes the last and then nothing is left to do.
        let wiped = raster.draw(&Scene::default()).unwrap();
        assert!(wiped.x0 <= 300 && wiped.x1 >= 350);
        assert!(raster.pixels().iter().all(|&b| b == 0));
        assert_eq!(raster.draw(&Scene::default()), None);
    }

    #[test]
    fn the_damage_covers_every_segment_box_and_stays_on_the_canvas() {
        let mut raster = Raster::new(400, 300, 1);
        let damage = raster
            .draw(&scene(vec![
                seg([10.0, 10.5], [60.0, 10.5], 1.0, 1.0),
                seg([300.0, 250.5], [395.0, 250.5], 1.0, 1.0),
                // Wholly off the canvas.
                seg([-500.0, -50.5], [-400.0, -50.5], 1.0, 1.0),
            ]))
            .unwrap();
        assert_eq!((damage.x0, damage.y0), (5, 5));
        assert_eq!(damage.x1, 400);
        assert!(damage.y1 >= 255 && damage.y1 <= 300);
        // A segment crossing the edge is clipped, not a panic.
        raster.draw(&scene(vec![seg([-20.0, 100.5], [20.0, 100.5], 1.0, 1.0)]));
        assert_eq!(raster.pixel(0, 100), RED_FULL);
    }

    #[test]
    fn four_hundred_segments_draw_in_well_under_a_frame_budget() {
        // A random walk of ~12 px steps, as drawing fast does.
        let (mut x, mut y, mut seed) = (900.0_f32, 500.0_f32, 777_u32);
        let mut rnd = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / 16_777_216.0 - 0.5
        };
        let segments: Vec<Segment> = (0..400)
            .map(|_| {
                let next = [
                    (x + rnd() * 24.0).clamp(10.0, 1_900.0),
                    (y + rnd() * 24.0).clamp(10.0, 1_070.0),
                ];
                let s = seg([x, y], next, 1.0, 1.0);
                (x, y) = (next[0], next[1]);
                s
            })
            .collect();
        let scene = scene(segments);
        let mut raster = Raster::new(1_920, 1_080, 1);
        // The first draw also allocates; the bound is generous (the spike measured 1.7 ms for
        // these at opt-level 2), so only an accidental full-screen loop trips it.
        let mut worst = std::time::Duration::ZERO;
        for _ in 0..5 {
            let started = std::time::Instant::now();
            raster.draw(&scene);
            worst = worst.max(started.elapsed());
        }
        assert!(worst < std::time::Duration::from_millis(20), "{worst:?}");
    }
}
