//! Win32 reader for the Accounts key row, separate from general clipboard handling.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use windows::Win32::Foundation::HGLOBAL;
use windows::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, GetClipboardSequenceNumber, GetPriorityClipboardFormat,
    OpenClipboard,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{
    CF_BITMAP, CF_DIB, CF_DIBV5, CF_ENHMETAFILE, CF_HDROP, CF_METAFILEPICT, CF_UNICODETEXT,
};
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
    let mixed = [
        CF_BITMAP,
        CF_DIB,
        CF_DIBV5,
        CF_ENHMETAFILE,
        CF_HDROP,
        CF_METAFILEPICT,
    ]
    .map(|format| u32::from(format.0));
    // SAFETY: the format list is valid; the query does not read payloads or format names.
    if unsafe { GetPriorityClipboardFormat(&mixed) } > 0 {
        return Err(ClipboardRefusal::Invalid);
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
    let ptr = unsafe { GlobalLock(text) }.cast::<u16>();
    if ptr.is_null() {
        return Err(ClipboardRefusal::Unavailable);
    }
    let _locked = Locked(text);
    // SAFETY: GlobalLock returns aligned memory; units derives from the allocation size.
    let bounded =
        unsafe { std::slice::from_raw_parts(ptr, units.min(MAX_KEY_CLIPBOARD_BYTES + 1)) };
    decode(bounded)
}

fn decode(units: &[u16]) -> KeyClipboardResult {
    let end = units.iter().position(|&unit| unit == 0).ok_or({
        if units.len() > MAX_KEY_CLIPBOARD_BYTES {
            ClipboardRefusal::TooLong
        } else {
            ClipboardRefusal::Invalid
        }
    })?;
    if end == 0 {
        return Err(ClipboardRefusal::Invalid);
    }
    let mut text = Zeroizing::new(String::with_capacity(MAX_KEY_CLIPBOARD_BYTES));
    for scalar in char::decode_utf16(units[..end].iter().copied()) {
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
    use super::*;

    #[test]
    fn clipboard_limits_raw_utf8_before_trimming() {
        let exact = format!("{} ", "é".repeat(1023)) + " ";
        let full = exact.encode_utf16().chain([0]).collect::<Vec<_>>();
        assert_eq!(decode(&full).expect("fits").as_str(), exact);
        let over = (exact + " ").encode_utf16().chain([0]).collect::<Vec<_>>();
        assert_eq!(decode(&over), Err(ClipboardRefusal::TooLong));
        assert_eq!(
            decode(&[u16::from(b'x'); 2049]),
            Err(ClipboardRefusal::TooLong)
        );
    }

    #[test]
    fn clipboard_requires_terminated_nonempty_valid_unicode() {
        for units in [&[][..], &[0], &[0xD800, 0], &[u16::from(b'x')]] {
            assert_eq!(decode(units), Err(ClipboardRefusal::Invalid));
        }
        assert_eq!(decode(&[0xD83D, 0xDE00, 0]).expect("pair").as_str(), "😀");
    }
}
