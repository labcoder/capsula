//! The hit proxy's per-pixel-alpha mask. Pixels inside the presence's hit band get alpha 1,
//! which is invisible but takes the pointer; every other pixel gets alpha 0, which Windows
//! passes through to whatever lies beneath, in any process. The proxy never gets
//! `WS_EX_TRANSPARENT` or a region, and the bitmap is allocated once.

use std::ffi::c_void;

use windows::Win32::Foundation::{COLORREF, HWND, POINT, SIZE};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, HBITMAP, HDC,
    HGDIOBJ, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{ULW_ALPHA, UpdateLayeredWindow};

use super::presence::{PresenceError, struct_size};
pub(crate) use crate::platform::MaskUpdate;
use crate::platform::{ROW_SPANS, RowSpans};

/// Premultiplied BGRA with alpha 1: invisible, but Windows hit-tests it.
const OWNED: u32 = 0x0100_0000;

/// A 32-bit top-down DIB in a memory DC, and the owned spans each of its rows holds.
#[derive(Debug)]
pub(super) struct HitMask {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    pixels: *mut u32,
    width: usize,
    /// One entry per bitmap row: up to [`ROW_SPANS`] owned column spans, `[left, right)`.
    rows: Vec<[Option<[usize; 2]>; ROW_SPANS]>,
    /// Where the proxy is, `[x, y, width, height]`; empty before the first update.
    window: [i32; 4],
}

impl HitMask {
    /// Allocates a transparent mask for proxies up to `[width, height]` physical pixels.
    ///
    /// # Errors
    /// Fails when the size is empty or Windows refuses the bitmap.
    pub(super) fn new([width, height]: [i32; 2]) -> Result<Self, PresenceError> {
        let (Ok(columns), Ok(rows)) = (usize::try_from(width), usize::try_from(height)) else {
            return Err(PresenceError::MaskSize);
        };
        if columns == 0 || rows == 0 {
            return Err(PresenceError::MaskSize);
        }
        // SAFETY: creates a memory DC compatible with the screen; this mask owns it.
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.is_invalid() {
            return Err(windows::core::Error::from_thread().into());
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: struct_size::<BITMAPINFOHEADER>(),
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        // SAFETY: info describes a 32-bit DIB; bits receives its zeroed pixels' address.
        let created = unsafe {
            CreateDIBSection(
                Some(dc),
                &raw const info,
                DIB_RGB_COLORS,
                &raw mut bits,
                None,
                0,
            )
        };
        let bitmap = match created {
            Ok(bitmap) if !bits.is_null() => bitmap,
            failed => {
                if let Ok(bitmap) = failed {
                    // SAFETY: the bitmap was created above and never selected.
                    let _deleted = unsafe { DeleteObject(bitmap.into()) };
                }
                // SAFETY: the DC was created above and holds nothing of ours yet.
                let _deleted = unsafe { DeleteDC(dc) };
                return Err(failed
                    .err()
                    .map_or(PresenceError::MaskSize, PresenceError::from));
            }
        };
        // SAFETY: selects a bitmap this mask owns into a DC it owns.
        let previous = unsafe { SelectObject(dc, bitmap.into()) };
        Ok(Self {
            dc,
            bitmap,
            previous,
            pixels: bits.cast::<u32>(),
            width: columns,
            rows: vec![[None; ROW_SPANS]; rows],
            window: [0; 4],
        })
    }

    /// Rewrites the rows whose owned spans changed, from `spans`, which gives a screen row's
    /// owned columns as up to [`ROW_SPANS`] `[left, right)` spans. Then one `UpdateLayeredWindow` hands
    /// Windows the pixels, and moves the proxy too when `window` differs from where it is.
    ///
    /// # Errors
    /// Fails when `window` is bigger than the mask or Windows refuses the update.
    pub(super) fn update(
        &mut self,
        hwnd: HWND,
        window: [i32; 4],
        spans: impl Fn(i32) -> RowSpans,
    ) -> Result<MaskUpdate, PresenceError> {
        let [x, y, width, height] = window;
        let (Ok(columns), Ok(rows)) = (usize::try_from(width), usize::try_from(height)) else {
            return Err(PresenceError::MaskSize);
        };
        if columns > self.width || rows > self.rows.len() {
            return Err(PresenceError::MaskSize);
        }
        // SAFETY: the DIB holds width x rows pixels for as long as this mask, and &mut self
        // makes this the only view of them.
        let pixels =
            unsafe { std::slice::from_raw_parts_mut(self.pixels, self.width * self.rows.len()) };
        let local = |span: Option<[i32; 2]>| {
            let [left, right] = span?;
            let left = usize::try_from((left - x).max(0)).ok()?;
            let right = usize::try_from((right - x).min(width)).ok()?;
            (left < right).then_some([left, right])
        };
        let mut changed = false;
        for (index, (held, line)) in self
            .rows
            .iter_mut()
            .zip(pixels.chunks_exact_mut(self.width))
            .enumerate()
        {
            let wanted = i32::try_from(index)
                .ok()
                .filter(|_| index < rows)
                .map_or([None; ROW_SPANS], |row| spans(y + row).map(local));
            if wanted == *held {
                continue;
            }
            for [left, right] in held.iter().flatten() {
                line[*left..*right].fill(0);
            }
            for [left, right] in wanted.iter().flatten() {
                line[*left..*right].fill(OWNED);
            }
            *held = wanted;
            changed = true;
        }
        let moved = window != self.window;
        if !moved && !changed {
            return Ok(MaskUpdate::Unchanged);
        }
        let position = POINT { x, y };
        let size = SIZE {
            cx: width,
            cy: height,
        };
        let source = POINT::default();
        let blend = BLENDFUNCTION {
            BlendOp: u8::try_from(AC_SRC_OVER).unwrap_or_default(),
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: u8::try_from(AC_SRC_ALPHA).unwrap_or(1),
        };
        // SAFETY: every pointer is to a local that outlives the call, the DC holds the DIB,
        // and hwnd is the caller's live layered window.
        let result = unsafe {
            UpdateLayeredWindow(
                hwnd,
                None,
                moved.then_some(&raw const position),
                moved.then_some(&raw const size),
                Some(self.dc),
                Some(&raw const source),
                COLORREF(0),
                Some(&raw const blend),
                ULW_ALPHA,
            )
        };
        if let Err(error) = result {
            // The next update sends every pixel and the position again.
            self.window = [0; 4];
            return Err(error.into());
        }
        self.window = window;
        Ok(if moved {
            MaskUpdate::Moved
        } else {
            MaskUpdate::Shape
        })
    }
}

impl Drop for HitMask {
    fn drop(&mut self) {
        // SAFETY: puts the DC's original bitmap back, so ours can be deleted.
        unsafe { SelectObject(self.dc, self.previous) };
        // SAFETY: the bitmap is no longer selected and is deleted once.
        if !unsafe { DeleteObject(self.bitmap.into()) }.as_bool() {
            tracing::debug!("can't delete the hit mask's bitmap");
        }
        // SAFETY: the DC is deleted once.
        if !unsafe { DeleteDC(self.dc) }.as_bool() {
            tracing::debug!("can't delete the hit mask's DC");
        }
    }
}
