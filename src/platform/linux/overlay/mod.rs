//! The ink overlay: a `wlr-layer-shell` surface over one output that shows what ⇧ draws, and
//! is in the video since `wf-recorder` records the composited output.

pub mod raster;
pub mod scene;
