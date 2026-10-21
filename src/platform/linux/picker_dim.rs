//! The target picker's dim, drawn by its own GPUI window: everything outside the highlighted
//! rect is dimmed, a window or screen gets a tint and a blue inset border, an area a thin white
//! one. The constants are those of `laser_overlay.metal` at the start of the picker (the
//! Metal overlay draws the same on macOS); the geometry is plain data so it can be tested
//! without a window.

use gpui::{Bounds, FillOptions, FillRule, PathBuilder, PathStyle, Pixels, Point, Window, point, px};

/// Kind of a picker rect (`TelemetryRect.1`): a dragged area.
pub const KIND_AREA: u8 = 1;
/// A window or a whole screen.
pub const KIND_WINDOW: u8 = 2;

/// The dim outside the target, at full fade.
const DIM_ALPHA: f32 = 0.32;
/// The tint inside a window or screen (`#3b5cf6`).
const TINT_RGB: u32 = 0x3b5cf6;
const TINT_ALPHA: f32 = 0.13;
const WINDOW_RADIUS: f32 = 10.0;
/// A window's or screen's border (`#5b7cfa`, 2 px) and an area's (white, 1 px).
const WINDOW_BORDER: (u32, f32, f32) = (0x5b7cfa, 0.93, 2.0);
const AREA_BORDER: (u32, f32, f32) = (0xffffff, 0.92, 1.0);
/// Control point distance of a quarter circle as a share of its radius.
const CUBIC_K: f32 = 0.5523;

/// A colour and its opacity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fill {
    pub rgb: u32,
    pub alpha: f32,
}

/// An inset border, `width` px wide, inside the cutout's edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Border {
    pub fill: Fill,
    pub width: f32,
}

/// What the picker paints, in window-local points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DimShape {
    /// The highlighted rect; `None` dims the whole screen.
    pub cutout: Option<[f32; 4]>,
    /// Corner radius of the cutout.
    pub radius: f32,
    pub dim: Fill,
    /// Over the cutout: a window's blue.
    pub tint: Option<Fill>,
    pub border: Option<Border>,
}

/// The dim for a picker rect of `kind` (`KIND_AREA`, `KIND_WINDOW`) on a screen of `screen`
/// points, `fade` (0..=1) into the picker's opening. A rect is clipped to the screen; one
/// under a pixel on a side is none. Any other kind has no cutout.
pub fn dim_shape(kind: u8, rect: Option<[f32; 4]>, screen: [f32; 2], fade: f32) -> DimShape {
    let fade = fade.clamp(0.0, 1.0);
    let cutout = rect
        .map(|r| clip(r, screen))
        .filter(|r| r[2] >= 1.0 && r[3] >= 1.0)
        .filter(|_| matches!(kind, KIND_AREA | KIND_WINDOW));
    let fill = |rgb, alpha: f32| Fill {
        rgb,
        alpha: alpha * fade,
    };
    let dim = fill(0x000000, DIM_ALPHA);
    let (Some(cutout), true) = (cutout, fade > 0.0) else {
        return DimShape {
            cutout,
            radius: 0.0,
            dim,
            tint: None,
            border: None,
        };
    };
    let window = kind == KIND_WINDOW;
    let (rgb, alpha, width) = if window { WINDOW_BORDER } else { AREA_BORDER };
    let radius = if window { WINDOW_RADIUS } else { 0.0 };
    DimShape {
        cutout: Some(cutout),
        radius: radius.min(cutout[2] / 2.0).min(cutout[3] / 2.0),
        dim,
        tint: window.then(|| fill(TINT_RGB, TINT_ALPHA)),
        border: Some(Border {
            fill: fill(rgb, alpha),
            width,
        }),
    }
}

fn clip([x, y, w, h]: [f32; 4], [sw, sh]: [f32; 2]) -> [f32; 4] {
    let x0 = x.clamp(0.0, sw);
    let y0 = y.clamp(0.0, sh);
    let x1 = (x + w).clamp(0.0, sw);
    let y1 = (y + h).clamp(0.0, sh);
    [x0, y0, x1 - x0, y1 - y0]
}

/// One step of an outline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Seg {
    Move([f32; 2]),
    Line([f32; 2]),
    /// To `to`, with control points `a` then `b`.
    Cubic {
        to: [f32; 2],
        a: [f32; 2],
        b: [f32; 2],
    },
    Close,
}

/// A rectangle with corners of `radius` (clamped to half the shorter side), clockwise from
/// the top edge. No curves at radius 0.
pub fn rounded_rect([x, y, w, h]: [f32; 4], radius: f32) -> Vec<Seg> {
    let r = radius.clamp(0.0, w.min(h) / 2.0);
    let (x1, y1) = (x + w, y + h);
    if r <= 0.0 {
        return vec![
            Seg::Move([x, y]),
            Seg::Line([x1, y]),
            Seg::Line([x1, y1]),
            Seg::Line([x, y1]),
            Seg::Close,
        ];
    }
    let k = CUBIC_K * r;
    vec![
        Seg::Move([x + r, y]),
        Seg::Line([x1 - r, y]),
        Seg::Cubic {
            to: [x1, y + r],
            a: [x1 - r + k, y],
            b: [x1, y + r - k],
        },
        Seg::Line([x1, y1 - r]),
        Seg::Cubic {
            to: [x1 - r, y1],
            a: [x1, y1 - r + k],
            b: [x1 - r + k, y1],
        },
        Seg::Line([x + r, y1]),
        Seg::Cubic {
            to: [x, y1 - r],
            a: [x + r - k, y1],
            b: [x, y1 - r + k],
        },
        Seg::Line([x, y + r]),
        Seg::Cubic {
            to: [x + r, y],
            a: [x, y + r - k],
            b: [x + r - k, y],
        },
        Seg::Close,
    ]
}

/// The dim as one outline to fill even-odd: the screen, then the cutout (a hole).
pub fn dim_outline(shape: &DimShape, screen: [f32; 2]) -> Vec<Seg> {
    let mut outline = rounded_rect([0.0, 0.0, screen[0], screen[1]], 0.0);
    if let Some(cutout) = shape.cutout {
        outline.extend(rounded_rect(cutout, shape.radius));
    }
    outline
}

/// The centre line of a border of `width` inside `rect`: stroked at `width`, it fills the
/// band from the edge inward.
pub fn border_outline([x, y, w, h]: [f32; 4], radius: f32, width: f32) -> Vec<Seg> {
    let inset = width / 2.0;
    rounded_rect(
        [x + inset, y + inset, w - width, h - width],
        (radius - inset).max(0.0),
    )
}

fn path(
    builder: PathBuilder,
    outline: &[Seg],
    origin: Point<Pixels>,
) -> Option<gpui::Path<Pixels>> {
    let mut builder = builder;
    let at = |p: [f32; 2]| origin + point(px(p[0]), px(p[1]));
    for seg in outline {
        match *seg {
            Seg::Move(p) => builder.move_to(at(p)),
            Seg::Line(p) => builder.line_to(at(p)),
            Seg::Cubic { to, a, b } => builder.cubic_bezier_to(at(to), at(a), at(b)),
            Seg::Close => builder.close(),
        }
    }
    builder.build().ok()
}

fn colour(fill: Fill) -> gpui::Rgba {
    let [r, g, b] = [16, 8, 0].map(|shift| ((fill.rgb >> shift) & 0xff) as f32 / 255.0);
    gpui::Rgba {
        r,
        g,
        b,
        a: fill.alpha,
    }
}

/// Paints `shape` over `bounds` (the picker's whole window): the dim, the tint and the border.
pub fn paint(window: &mut Window, bounds: Bounds<Pixels>, shape: &DimShape) {
    let screen = [
        f32::from(bounds.size.width),
        f32::from(bounds.size.height),
    ];
    let even_odd = || {
        PathBuilder::fill().with_style(PathStyle::Fill(
            FillOptions::default().with_fill_rule(FillRule::EvenOdd),
        ))
    };
    if shape.dim.alpha > 0.0
        && let Some(dim) = path(even_odd(), &dim_outline(shape, screen), bounds.origin)
    {
        window.paint_path(dim, colour(shape.dim));
    }
    let Some(cutout) = shape.cutout else { return };
    if let Some(tint) = shape.tint
        && tint.alpha > 0.0
        && let Some(fill) = path(
            PathBuilder::fill(),
            &rounded_rect(cutout, shape.radius),
            bounds.origin,
        )
    {
        window.paint_path(fill, colour(tint));
    }
    if let Some(border) = shape.border
        && border.fill.alpha > 0.0
        && let Some(stroke) = path(
            PathBuilder::stroke(px(border.width)),
            &border_outline(cutout, shape.radius, border.width),
            bounds.origin,
        )
    {
        window.paint_path(stroke, colour(border.fill));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: [f32; 2] = [1920.0, 1080.0];

    #[test]
    fn a_window_is_dimmed_around_with_a_tint_and_a_blue_border() {
        let rect = [200.0, 100.0, 800.0, 500.0];
        let shape = dim_shape(KIND_WINDOW, Some(rect), SCREEN, 1.0);
        assert_eq!(shape.cutout, Some(rect));
        assert_eq!(shape.radius, 10.0);
        assert_eq!(shape.dim, Fill { rgb: 0, alpha: 0.32 });
        assert_eq!(
            shape.tint,
            Some(Fill {
                rgb: 0x3b5cf6,
                alpha: 0.13
            })
        );
        assert_eq!(
            shape.border,
            Some(Border {
                fill: Fill {
                    rgb: 0x5b7cfa,
                    alpha: 0.93
                },
                width: 2.0
            })
        );
    }

    #[test]
    fn an_area_has_square_corners_no_tint_and_a_thin_white_border() {
        let rect = [50.0, 60.0, 300.0, 200.0];
        let shape = dim_shape(KIND_AREA, Some(rect), SCREEN, 1.0);
        assert_eq!(shape.cutout, Some(rect));
        assert_eq!(shape.radius, 0.0);
        assert_eq!(shape.tint, None);
        assert_eq!(
            shape.border,
            Some(Border {
                fill: Fill {
                    rgb: 0xffffff,
                    alpha: 0.92
                },
                width: 1.0
            })
        );
    }

    #[test]
    fn no_rect_dims_the_whole_screen_evenly() {
        let shape = dim_shape(KIND_AREA, None, SCREEN, 1.0);
        assert_eq!(shape.cutout, None);
        assert_eq!(shape.dim.alpha, 0.32);
        assert_eq!((shape.tint, shape.border), (None, None));
        // A kind with no cutout of its own (the iPhone's) is the same dim.
        assert_eq!(dim_shape(3, Some([1.0, 1.0, 9.0, 9.0], ), SCREEN, 1.0).cutout, None);
    }

    #[test]
    fn the_fade_scales_every_opacity_and_zero_draws_nothing() {
        let rect = Some([200.0, 100.0, 800.0, 500.0]);
        let half = dim_shape(KIND_WINDOW, rect, SCREEN, 0.5);
        assert!((half.dim.alpha - 0.16).abs() < 1e-6);
        assert!((half.tint.unwrap().alpha - 0.065).abs() < 1e-6);
        assert!((half.border.unwrap().fill.alpha - 0.465).abs() < 1e-6);
        let none = dim_shape(KIND_WINDOW, rect, SCREEN, 0.0);
        assert_eq!(none.dim.alpha, 0.0);
        assert_eq!((none.tint, none.border), (None, None));
        // Past the ends is the ends.
        assert_eq!(dim_shape(KIND_WINDOW, rect, SCREEN, 7.0), dim_shape(KIND_WINDOW, rect, SCREEN, 1.0));
    }

    #[test]
    fn a_rect_is_clipped_to_the_screen_and_a_sub_pixel_one_is_none() {
        let shape = dim_shape(KIND_WINDOW, Some([-50.0, 1000.0, 300.0, 200.0]), SCREEN, 1.0);
        assert_eq!(shape.cutout, Some([0.0, 1000.0, 250.0, 80.0]));
        let off = dim_shape(KIND_WINDOW, Some([2000.0, 10.0, 100.0, 100.0]), SCREEN, 1.0);
        assert_eq!(off.cutout, None);
        let thin = dim_shape(KIND_AREA, Some([10.0, 10.0, 0.5, 300.0]), SCREEN, 1.0);
        assert_eq!(thin.cutout, None);
        assert_eq!(thin.dim.alpha, 0.32);
    }

    #[test]
    fn a_tiny_window_keeps_its_corners_inside_it() {
        let shape = dim_shape(KIND_WINDOW, Some([10.0, 10.0, 8.0, 30.0]), SCREEN, 1.0);
        assert_eq!(shape.radius, 4.0);
    }

    #[test]
    fn the_rounded_rect_is_four_lines_and_four_quarter_circle_cubics() {
        let path = rounded_rect([200.0, 200.0, 600.0, 300.0], 10.0);
        let k = 5.523;
        assert_eq!(path[0], Seg::Move([210.0, 200.0]));
        assert_eq!(path[1], Seg::Line([790.0, 200.0]));
        let Seg::Cubic { to, a, b } = path[2] else {
            panic!("{:?}", path[2]);
        };
        assert_eq!(to, [800.0, 210.0]);
        assert!((a[0] - (790.0 + k)).abs() < 1e-3 && a[1] == 200.0, "{a:?}");
        assert!(b[0] == 800.0 && (b[1] - (210.0 - k)).abs() < 1e-3, "{b:?}");
        assert_eq!(path[3], Seg::Line([800.0, 490.0]));
        // The last curve comes back to where the outline began.
        let Seg::Cubic { to, .. } = path[8] else {
            panic!("{:?}", path[8]);
        };
        assert_eq!(to, [210.0, 200.0]);
        assert_eq!(path[9], Seg::Close);
        assert_eq!(path.len(), 10);
    }

    #[test]
    fn a_square_rect_has_no_curves_and_the_radius_is_clamped() {
        let square = rounded_rect([1.0, 2.0, 30.0, 40.0], 0.0);
        assert_eq!(
            square,
            vec![
                Seg::Move([1.0, 2.0]),
                Seg::Line([31.0, 2.0]),
                Seg::Line([31.0, 42.0]),
                Seg::Line([1.0, 42.0]),
                Seg::Close
            ]
        );
        let capped = rounded_rect([0.0, 0.0, 20.0, 100.0], 50.0);
        assert_eq!(capped[0], Seg::Move([10.0, 0.0]));
    }

    #[test]
    fn the_dim_is_the_screen_with_the_cutout_as_a_second_outline() {
        let shape = dim_shape(KIND_WINDOW, Some([200.0, 200.0, 600.0, 300.0]), SCREEN, 1.0);
        let outline = dim_outline(&shape, SCREEN);
        assert_eq!(outline.len(), 5 + 10);
        assert_eq!(outline[0], Seg::Move([0.0, 0.0]));
        assert_eq!(outline[2], Seg::Line([1920.0, 1080.0]));
        assert_eq!(outline[5], Seg::Move([210.0, 200.0]));
        let uniform = dim_shape(KIND_AREA, None, SCREEN, 1.0);
        assert_eq!(dim_outline(&uniform, SCREEN).len(), 5);
    }

    #[test]
    fn the_border_is_stroked_along_a_line_half_its_width_inside_the_edge() {
        let rect = [200.0, 200.0, 600.0, 300.0];
        // 1 px: the line runs at +0.5, and a corner of radius 10 becomes 9.5.
        let thin = border_outline(rect, 0.0, 1.0);
        assert_eq!(thin[0], Seg::Move([200.5, 200.5]));
        assert_eq!(thin[2], Seg::Line([799.5, 499.5]));
        // 2 px: at +1, corners of radius 9.
        let wide = border_outline(rect, 10.0, 2.0);
        assert_eq!(wide[0], Seg::Move([210.0, 201.0]));
        assert_eq!(wide[1], Seg::Line([790.0, 201.0]));
    }
}
