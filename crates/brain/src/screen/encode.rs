//! Portable overview resampling and native focus crops, exclusively from sanitized pixels.

use std::io::Write;

use image::ExtendedColorType;
use image::codecs::jpeg::JpegEncoder;
use kumochi_protocol::geometry::{LogicalPoint, OverviewPoint, PhysicalPoint, PhysicalRect};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::capture::Failure;
use super::mask::Completeness;
use super::sanitize::{SanitizedFrame, intersection};
use super::wire::{CaptureId, Refusal};

const OVERVIEW_LIMIT: [u32; 2] = [1280, 720];
const CROP_LIMIT: [u32; 2] = [1024, 576];
const JPEG_QUALITY: u8 = 85;
const ENCODE_FAULT: i32 = -2_147_467_259;
/// The largest JPEG the worker hands out; one over refuses, with no further downscale.
// shortcut: 2 MiB is a guess, not a measurement; set it from M10's JPEG sizes of real windows.
pub(crate) const JPEG_CAP: usize = 2 * 1024 * 1024;

/// Coordinate spaces belonging to one image, all relative to its captured display.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CaptureTransform {
    pub(crate) display: PhysicalRect,
    pub(crate) dpi: u32,
    pub(crate) overview_size: [u32; 2],
    pub(crate) crop: Option<PhysicalRect>,
}

#[cfg_attr(not(test), expect(dead_code, reason = "A6 consumes image transforms"))]
impl CaptureTransform {
    /// Rounds to the nearest physical pixel; half pixels round away from zero.
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "rounded float coordinates intentionally saturate to the physical i32 range"
    )]
    pub(crate) fn overview_to_physical(self, point: OverviewPoint) -> PhysicalPoint {
        PhysicalPoint(std::array::from_fn(|axis| {
            (f64::from(self.display.origin[axis])
                + f64::from(point.0[axis]) * f64::from(self.display.size[axis])
                    / f64::from(self.overview_size[axis]))
            .round() as i32
        }))
    }

    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "the protocol represents fractional overview coordinates as f32"
    )]
    pub(crate) fn physical_to_overview(self, point: PhysicalPoint) -> Option<OverviewPoint> {
        for axis in 0..2 {
            let offset = i64::from(point.0[axis]) - i64::from(self.display.origin[axis]);
            if offset < 0 || offset >= i64::from(self.display.size[axis]) {
                return None;
            }
        }
        Some(OverviewPoint(std::array::from_fn(|axis| {
            ((f64::from(point.0[axis]) - f64::from(self.display.origin[axis]))
                * f64::from(self.overview_size[axis])
                / f64::from(self.display.size[axis])) as f32
        })))
    }

    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "the protocol represents fractional logical coordinates as f32"
    )]
    pub(crate) fn physical_to_logical(self, point: PhysicalPoint) -> LogicalPoint {
        LogicalPoint(std::array::from_fn(|axis| {
            ((f64::from(point.0[axis]) - f64::from(self.display.origin[axis])) * 96.0
                / f64::from(self.dpi)) as f32
        }))
    }

    pub(crate) fn crop_to_physical(self, point: PhysicalPoint) -> Option<PhysicalPoint> {
        let crop = self.crop?;
        Some(PhysicalPoint([
            crop.origin[0].saturating_add(point.0[0]),
            crop.origin[1].saturating_add(point.0[1]),
        ]))
    }
}

/// One JPEG. Its bytes are user content and never appear in diagnostics.
pub(crate) struct Encoded {
    pub(crate) jpeg: Zeroizing<Vec<u8>>,
    pub(crate) size: [u32; 2],
    #[cfg_attr(not(test), expect(dead_code, reason = "A6 checks image capture ids"))]
    pub(crate) capture: CaptureId,
    pub(crate) generation: u32,
    #[cfg_attr(not(test), expect(dead_code, reason = "A6 reports mask completeness"))]
    pub(crate) completeness: Completeness,
    #[cfg_attr(not(test), expect(dead_code, reason = "A6 carries image transforms"))]
    pub(crate) transform: CaptureTransform,
}

impl std::fmt::Debug for Encoded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Encoded")
            .field("bytes", &self.jpeg.len())
            .field("size", &self.size)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

/// Area-averages to fit 1280×720 without enlarging, then encodes RGB at quality 85.
///
/// # Errors
/// `TooLarge` over [`JPEG_CAP`]; a content-free numeric fault if JPEG encoding fails.
#[cfg_attr(not(test), expect(dead_code, reason = "A6 stages the overview"))]
pub(crate) fn overview(frame: &SanitizedFrame<'_>) -> Result<Encoded, Failure> {
    overview_within(frame, JPEG_CAP)
}

/// [`overview`] under a cap of `cap` JPEG bytes.
pub(crate) fn overview_within(frame: &SanitizedFrame<'_>, cap: usize) -> Result<Encoded, Failure> {
    let (meta, _) = frame.image();
    let size = overview_size(meta.display.size);
    encode(frame, meta.display, size, None, cap)
}

/// Clips and centres a native-pixel window no larger than 1024×576, without scaling.
///
/// # Errors
/// `Invalid` for an empty intersection; a numeric fault if JPEG encoding fails.
#[cfg_attr(not(test), expect(dead_code, reason = "A6 stages the focus crop"))]
pub(crate) fn crop(frame: &SanitizedFrame<'_>, region: PhysicalRect) -> Result<Encoded, Failure> {
    let (meta, _) = frame.image();
    let region = crop_rect(region, meta.display).ok_or(Refusal::Invalid)?;
    encode(frame, region, region.size, Some(region), JPEG_CAP)
}

fn overview_size(size: [u32; 2]) -> [u32; 2] {
    let [w, h] = size;
    if w <= OVERVIEW_LIMIT[0] && h <= OVERVIEW_LIMIT[1] {
        return size;
    }
    if w * OVERVIEW_LIMIT[1] >= h * OVERVIEW_LIMIT[0] {
        [OVERVIEW_LIMIT[0], (h * OVERVIEW_LIMIT[0] / w).max(1)]
    } else {
        [(w * OVERVIEW_LIMIT[1] / h).max(1), OVERVIEW_LIMIT[1]]
    }
}

fn crop_rect(region: PhysicalRect, display: PhysicalRect) -> Option<PhysicalRect> {
    let mut rect = intersection(region, display)?;
    for (axis, limit) in CROP_LIMIT.into_iter().enumerate() {
        let size = rect.size[axis].min(limit);
        let centred = i64::from(rect.origin[axis]) + i64::from((rect.size[axis] - size) / 2);
        let last = i64::from(display.origin[axis]) + i64::from(display.size[axis] - size);
        rect.origin[axis] = i32::try_from(centred.clamp(i64::from(display.origin[axis]), last))
            .expect("crop is inside a captured display");
        rect.size[axis] = size;
    }
    Some(rect)
}

/// Writes into a buffer made with its whole capacity, so the JPEG is never copied by a
/// reallocation, and remembers whether the encoder wanted more than the cap.
struct Capped<'a> {
    bytes: &'a mut Vec<u8>,
    cap: usize,
    over: bool,
}

impl Write for Capped<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.len() > self.cap - self.bytes.len() {
            self.over = true;
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encode(
    frame: &SanitizedFrame<'_>,
    region: PhysicalRect,
    size: [u32; 2],
    crop: Option<PhysicalRect>,
    cap: usize,
) -> Result<Encoded, Failure> {
    let rgb = Zeroizing::new(resample(frame, region, size));
    let mut jpeg = Zeroizing::new(Vec::with_capacity(cap));
    let mut out = Capped {
        bytes: &mut jpeg,
        cap,
        over: false,
    };
    let encoded = JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY).encode(
        &rgb,
        size[0],
        size[1],
        ExtendedColorType::Rgb8,
    );
    if out.over {
        return Err(Refusal::TooLarge.into());
    }
    encoded.map_err(|_| Failure::Fault(ENCODE_FAULT))?;
    let (meta, _) = frame.image();
    Ok(Encoded {
        jpeg,
        size,
        capture: meta.capture,
        generation: frame.generation(),
        completeness: frame.completeness(),
        transform: CaptureTransform {
            display: meta.display,
            dpi: meta.dpi,
            overview_size: overview_size(meta.display.size),
            crop,
        },
    })
}

fn resample(frame: &SanitizedFrame<'_>, region: PhysicalRect, size: [u32; 2]) -> Vec<u8> {
    let (meta, bgra) = frame.image();
    let [width, _] = meta.display.size.map(u64::from);
    let [sw, sh] = region.size.map(u64::from);
    let [dw, dh] = size.map(u64::from);
    let [left, top] = std::array::from_fn(|axis| {
        u64::try_from(i64::from(region.origin[axis]) - i64::from(meta.display.origin[axis]))
            .expect("encoding region lies inside the display")
    });
    let mut rgb = Vec::with_capacity(usize::try_from(dw * dh * 3).expect("bounded RGB length"));
    for y in 0..dh {
        let (y0, y1) = (y * sh, (y + 1) * sh);
        for x in 0..dw {
            let (x0, x1) = (x * sw, (x + 1) * sw);
            let mut sums = [0_u64; 3];
            for sy in y0 / dh..y1.div_ceil(dh) {
                let wy = y1.min((sy + 1) * dh) - y0.max(sy * dh);
                for sx in x0 / dw..x1.div_ceil(dw) {
                    let wx = x1.min((sx + 1) * dw) - x0.max(sx * dw);
                    let offset = usize::try_from(((top + sy) * width + left + sx) * 4)
                        .expect("bounded BGRA offset");
                    for channel in 0..3 {
                        sums[channel] += u64::from(bgra[offset + 2 - channel]) * wx * wy;
                    }
                }
            }
            for sum in sums {
                rgb.push(
                    u8::try_from((sum + sw * sh / 2) / (sw * sh)).expect("weighted mean of bytes"),
                );
            }
        }
    }
    rgb
}

#[cfg(test)]
#[path = "encode/tests.rs"]
mod tests;
