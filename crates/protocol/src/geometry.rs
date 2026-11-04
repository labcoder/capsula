//! Physical desktop coordinates and opaque native window references shared by capture messages.

use serde::{Deserialize, Serialize};

/// A rectangle in physical desktop pixels; its origin may be on a negative-coordinate display.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalRect {
    /// Top-left desktop coordinate, in physical pixels.
    pub origin: [i32; 2],
    /// Width and height, in physical pixels.
    pub size: [u32; 2],
}

/// The physical bounds of the display containing the pointer when a hold was recognized.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayRef(pub PhysicalRect);

/// An opaque native window reference: an HWND on Windows or a CGWindowID on macOS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowRef(pub u64);
