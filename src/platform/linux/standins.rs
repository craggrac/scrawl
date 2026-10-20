//! What macOS has and Linux doesn't yet: the Metal ink overlay (phase 5), the iPhone mirror and
//! the native capsule. Each is an empty enum: it can't be made, `open`/`start` say why, and the
//! code holding one is never reached.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};

use super::SharedCaptureTelemetry;
use crate::helper::PhoneDevice;
use crate::phone::PhoneRecording;

/// The ink and laser overlay.
pub enum MetalLaserOverlay {}

impl MetalLaserOverlay {
    pub fn open(
        _telemetry: Arc<SharedCaptureTelemetry>,
        _on: Option<[f64; 4]>,
    ) -> Result<Self, String> {
        Err("the ink overlay isn't available on Linux yet".into())
    }

    pub fn window_number(&self) -> u32 {
        match *self {}
    }

    pub fn screen(&self) -> [f64; 4] {
        match *self {}
    }

    pub fn move_to(&mut self, _rect: [f64; 4]) -> bool {
        match *self {}
    }
}

/// The cabled iPhone's mirror window and recording.
pub enum PhoneSession {}

impl PhoneSession {
    pub fn start_preview(
        _device_id: Option<&str>,
        _hidden: bool,
        _telemetry: Option<Arc<SharedCaptureTelemetry>>,
    ) -> Result<Self> {
        bail!("iPhone capture isn't available on Linux")
    }

    pub fn start(
        _movie: &Path,
        _device_id: Option<&str>,
        _telemetry: Option<Arc<SharedCaptureTelemetry>>,
    ) -> Result<Self> {
        bail!("iPhone capture isn't available on Linux")
    }

    pub fn begin_recording(&mut self, _movie: &Path) {
        match *self {}
    }

    pub fn show_preview(&mut self) {
        match *self {}
    }

    pub fn hide_preview(&mut self) {
        match *self {}
    }

    pub fn device(&self) -> Option<PhoneDevice> {
        match *self {}
    }

    pub fn rect(&self) -> Option<[f32; 4]> {
        match *self {}
    }

    pub fn elapsed_ms(&self) -> u64 {
        match *self {}
    }

    pub fn stop(self) -> Result<PhoneRecording> {
        match self {}
    }

    pub fn cancel(self) {
        match self {}
    }
}

/// The Metal capsule (`handoff::native_pill()` is always false on Linux).
pub enum NativePill {}

/// What a click on the native capsule asks for.
pub enum PillAction {}

impl NativePill {
    pub fn place_on(&self, _point: [f64; 2]) {
        match *self {}
    }

    pub fn drain_actions(&self) -> Vec<PillAction> {
        match *self {}
    }

    pub fn is_fully_hidden(&self) -> bool {
        match *self {}
    }

    pub fn pointer_on_pill(&self) -> bool {
        match *self {}
    }
}
