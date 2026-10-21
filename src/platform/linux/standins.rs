//! What macOS has and Linux doesn't: the iPhone mirror and the native capsule. Each is an empty
//! enum: it can't be made, `start` says why, and the code holding one is never reached.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};

use super::SharedCaptureTelemetry;
use crate::helper::PhoneDevice;
use crate::phone::PhoneRecording;

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
