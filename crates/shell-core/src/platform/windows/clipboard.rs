//! Win32 reader for the Accounts key row, separate from general clipboard handling.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use windows::Win32::Foundation::HGLOBAL;
use windows::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use zeroize::Zeroizing;

use super::super::clipboard::{ClipboardRefusal, KeyClipboardResult, MAX_KEY_CLIPBOARD_BYTES};

static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

struct InFlight;

impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT.store(false, Ordering::Release);
    }
}

struct Open;

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: only a successful open creates this guard, on the calling thread.
        let _closed = unsafe { CloseClipboard() };
    }
}

struct Locked(HGLOBAL);

impl Drop for Locked {
    fn drop(&mut self) {
        // SAFETY: the handle was locked once and the clipboard is still open.
        let _unlocked = unsafe { GlobalUnlock(self.0) };
    }
}

pub(in crate::platform) fn sequence() -> u32 {
    // SAFETY: this asks only for window-station metadata, with no pointer or data transfer.
    unsafe { GetClipboardSequenceNumber() }
}

pub(in crate::platform) fn read(
    expected: u32,
    deadline: Instant,
    is_current: impl Fn() -> bool,
) -> KeyClipboardResult {
    let live = || Instant::now() < deadline && is_current();
    if !live() || IN_FLIGHT.swap(true, Ordering::Acquire) {
        return Err(ClipboardRefusal::Unavailable);
    }
    let _in_flight = InFlight;
    // SAFETY: this read-only open needs no window owner; Open closes on this thread.
    unsafe { OpenClipboard(None) }.map_err(|_| ClipboardRefusal::Unavailable)?;
    let _open = Open;
    if sequence() != expected {
        return Err(ClipboardRefusal::Changed);
    }
    if !live() {
        return Err(ClipboardRefusal::Unavailable);
    }
    // SAFETY: the clipboard is open and this requests only its standard Unicode text.
    let text = unsafe { GetClipboardData(u32::from(CF_UNICODETEXT.0)) }
        .map_err(|_| ClipboardRefusal::Invalid)?;
    if !live() {
        return Err(ClipboardRefusal::Unavailable);
    }
    if sequence() != expected {
        return Err(ClipboardRefusal::Changed);
    }
    let text = HGLOBAL(text.0);
    // SAFETY: the clipboard owns a live global allocation for CF_UNICODETEXT.
    let units = unsafe { GlobalSize(text) } / size_of::<u16>();
    if units == 0 {
        return Err(ClipboardRefusal::Invalid);
    }
    // SAFETY: the clipboard owns this live handle until Open closes it.
    let ptr = NonNull::new(unsafe { GlobalLock(text) }.cast::<u16>())
        .ok_or(ClipboardRefusal::Unavailable)?;
    let _locked = Locked(text);
    // SAFETY: units comes from GlobalSize and bounds the reads to the allocation.
    // CF_UNICODETEXT initializes the string through its NUL, not its trailing capacity.
    // GlobalLock provides alignment for u16.
    // The clipboard is open and the guard holds the allocation locked until decoding returns.
    unsafe { decode_locked(ptr, units) }
}

/// # Safety
/// The pointer must be aligned and readable for `units` u16s while this runs.
/// The units through the first NUL, or the whole scanned bound if none, must be initialized.
unsafe fn decode_locked(ptr: NonNull<u16>, units: usize) -> KeyClipboardResult {
    let bound = units.min(MAX_KEY_CLIPBOARD_BYTES + 1);
    for end in 0..bound {
        // SAFETY: the bound is within the allocation and all earlier units were nonzero,
        // so this aligned u16 is part of the initialized prefix and is still readable.
        if unsafe { ptr.as_ptr().wrapping_add(end).read() } == 0 {
            // SAFETY: end is within the allocation; each preceding aligned u16 was read.
            // The resulting slice exists only for decoding, while the allocation is locked.
            let initialized = unsafe { std::slice::from_raw_parts(ptr.as_ptr(), end) };
            return decode(initialized);
        }
    }
    Err(if units > MAX_KEY_CLIPBOARD_BYTES {
        ClipboardRefusal::TooLong
    } else {
        ClipboardRefusal::Invalid
    })
}

fn decode(units: &[u16]) -> KeyClipboardResult {
    if units.is_empty() {
        return Err(ClipboardRefusal::Invalid);
    }
    let mut text = Zeroizing::new(String::with_capacity(MAX_KEY_CLIPBOARD_BYTES));
    for scalar in char::decode_utf16(units.iter().copied()) {
        let scalar = scalar.map_err(|_| ClipboardRefusal::Invalid)?;
        if text.len() + scalar.len_utf8() > MAX_KEY_CLIPBOARD_BYTES {
            return Err(ClipboardRefusal::TooLong);
        }
        text.push(scalar);
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use std::mem::MaybeUninit;

    use super::*;

    fn decode_block(units: &[u16]) -> KeyClipboardResult {
        // SAFETY: the borrowed slice is aligned, initialized and live for this call.
        unsafe { decode_locked(NonNull::from(units).cast(), units.len()) }
    }

    #[test]
    fn clipboard_stops_before_uninitialized_padding() {
        let block = [
            MaybeUninit::new(u16::from(b'x')),
            MaybeUninit::new(0),
            MaybeUninit::uninit(),
        ];
        // SAFETY: the live array is aligned for u16 and initialized through the first NUL.
        let text = unsafe { decode_locked(NonNull::from(&block).cast(), block.len()) };
        assert_eq!(text.expect("short text").as_str(), "x");
    }

    #[test]
    fn clipboard_limits_raw_utf8_before_trimming() {
        let mut exact = "é".repeat(1023);
        exact.push_str("  ");
        let full = exact.encode_utf16().chain([0]).collect::<Vec<_>>();
        assert_eq!(decode_block(&full).expect("fits").as_str(), exact);
        exact.push(' ');
        let over = exact.encode_utf16().chain([0]).collect::<Vec<_>>();
        assert_eq!(decode_block(&over), Err(ClipboardRefusal::TooLong));
        let mut ascii = [u16::from(b'x'); MAX_KEY_CLIPBOARD_BYTES + 2];
        assert_eq!(decode_block(&ascii), Err(ClipboardRefusal::TooLong));
        ascii[MAX_KEY_CLIPBOARD_BYTES + 1] = 0;
        assert_eq!(decode_block(&ascii), Err(ClipboardRefusal::TooLong));
        ascii[MAX_KEY_CLIPBOARD_BYTES] = 0;
        assert_eq!(
            decode_block(&ascii).expect("fits").as_str(),
            "x".repeat(MAX_KEY_CLIPBOARD_BYTES)
        );
    }

    #[test]
    fn clipboard_requires_terminated_nonempty_valid_unicode() {
        for units in [&[][..], &[0], &[0xD800, 0], &[u16::from(b'x')]] {
            assert_eq!(decode_block(units), Err(ClipboardRefusal::Invalid));
        }
        assert_eq!(
            decode_block(&[0xD83D, 0xDE00, 0]).expect("pair").as_str(),
            "😀"
        );
    }
}
