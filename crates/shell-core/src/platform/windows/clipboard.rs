//! Win32 reader for the Accounts key row, separate from general clipboard handling.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use windows::Win32::Foundation::HGLOBAL;
use windows::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Threading::GetCurrentProcess;
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
    let ptr = NonNull::new(unsafe { GlobalLock(text) }).ok_or(ClipboardRefusal::Unavailable)?;
    let _locked = Locked(text);
    copy_and_decode(ptr, units)
}

fn copy_and_decode(source: NonNull<c_void>, units: usize) -> KeyClipboardResult {
    let mut buffer = Zeroizing::new(vec![0u16; units.min(MAX_KEY_CLIPBOARD_BYTES + 1)]);
    let bytes = size_of_val(buffer.as_slice());
    let mut copied = 0;
    // SAFETY: this returns the pseudo-handle for this process and needs no inputs.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: Windows checks the source range in this process; Rust never dereferences it.
    // The destination is writable for the requested byte count, and copied is a separate output.
    unsafe {
        ReadProcessMemory(
            process,
            source.as_ptr(),
            buffer.as_mut_ptr().cast(),
            bytes,
            Some(&raw mut copied),
        )
    }
    .map_err(|_| ClipboardRefusal::Unavailable)?;
    if copied != bytes {
        return Err(ClipboardRefusal::Unavailable);
    }
    decode(&buffer)
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
    fn clipboard_decodes_only_terminated_text_in_the_bounded_native_copy() {
        let mut block = [u16::from(b'x'); MAX_KEY_CLIPBOARD_BYTES + 2];
        block[MAX_KEY_CLIPBOARD_BYTES + 1] = 0;
        assert_eq!(
            copy_and_decode(NonNull::from(&block).cast(), block.len()),
            Err(ClipboardRefusal::TooLong)
        );
        block[1] = 0;
        block[2] = 0xD800;
        assert_eq!(
            copy_and_decode(NonNull::from(&block).cast(), block.len())
                .expect("short text")
                .as_str(),
            "x"
        );
    }

    #[test]
    fn clipboard_limits_raw_utf8_before_trimming() {
        let mut exact = "é".repeat(1023);
        exact.push_str("  ");
        let full = exact.encode_utf16().chain([0]).collect::<Vec<_>>();
        assert_eq!(decode(&full).expect("fits").as_str(), exact);
        exact.push(' ');
        let over = exact.encode_utf16().chain([0]).collect::<Vec<_>>();
        assert_eq!(decode(&over), Err(ClipboardRefusal::TooLong));
        let mut ascii = [u16::from(b'x'); MAX_KEY_CLIPBOARD_BYTES + 1];
        assert_eq!(decode(&ascii), Err(ClipboardRefusal::TooLong));
        ascii[MAX_KEY_CLIPBOARD_BYTES] = 0;
        assert_eq!(
            decode(&ascii).expect("fits").as_str(),
            "x".repeat(MAX_KEY_CLIPBOARD_BYTES)
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
