//! The seam between the shared GPUI app (`crate::ui`) and the operating system: the capture
//! session, input monitoring, targets and screens, and the stand-ins for what one platform
//! doesn't have yet, all under the names `ui` uses. macOS re-exports its own code; Linux
//! implements the same surface on Hyprland and the `crate::linux` recorders.

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::*;
