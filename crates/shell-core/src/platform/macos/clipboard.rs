//! Reads one bounded string from the general pasteboard for an explicit key paste.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use objc2::rc::Retained;
use objc2_app_kit::{NSPasteboard, NSPasteboardAccessBehavior, NSPasteboardTypeString};
use objc2_foundation::{NSString, NSUTF8StringEncoding};
use zeroize::Zeroizing;

use super::super::clipboard::{ClipboardRefusal, KeyClipboardResult, MAX_KEY_CLIPBOARD_BYTES};

static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

struct InFlight;

impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT.store(false, Ordering::Release);
    }
}

pub(in crate::platform) fn count() -> Result<isize, ClipboardRefusal> {
    Ok(general()?.changeCount())
}

pub(in crate::platform) fn read(
    expected: isize,
    deadline: Instant,
    is_current: impl Fn() -> bool,
) -> KeyClipboardResult {
    let board = general()?;
    read_from(&board, expected, deadline, is_current)
}

/// The general pasteboard, unless the user set Kumochi never to read it.
fn general() -> Result<Retained<NSPasteboard>, ClipboardRefusal> {
    let board = NSPasteboard::generalPasteboard();
    if objc2::available!(macos = 15.4)
        && board.accessBehavior() == NSPasteboardAccessBehavior::AlwaysDeny
    {
        return Err(ClipboardRefusal::Unavailable);
    }
    Ok(board)
}

fn read_from(
    board: &NSPasteboard,
    expected: isize,
    deadline: Instant,
    is_current: impl Fn() -> bool,
) -> KeyClipboardResult {
    let live = || Instant::now() < deadline && is_current();
    if !live() || IN_FLIGHT.swap(true, Ordering::Acquire) {
        return Err(ClipboardRefusal::Unavailable);
    }
    let _in_flight = InFlight;
    if board.changeCount() != expected {
        return Err(ClipboardRefusal::Changed);
    }
    if board
        .pasteboardItems()
        .is_none_or(|items| items.count() != 1)
    {
        return Err(ClipboardRefusal::Invalid);
    }
    // SAFETY: AppKit's string type constant, which never changes while the process runs.
    let kind = unsafe { NSPasteboardTypeString };
    let text = board.stringForType(kind).ok_or(ClipboardRefusal::Invalid)?;
    if !live() {
        return Err(ClipboardRefusal::Unavailable);
    }
    if board.changeCount() != expected {
        return Err(ClipboardRefusal::Changed);
    }
    copy(&text)
}

/// Copies the string's UTF-8 into a zeroizing buffer, bounded before any copy is made.
fn copy(text: &NSString) -> KeyClipboardResult {
    // A UTF-16 unit is never fewer than one UTF-8 byte, so this bounds the work first.
    if text.length() > MAX_KEY_CLIPBOARD_BYTES {
        return Err(ClipboardRefusal::TooLong);
    }
    let bytes = text.lengthOfBytesUsingEncoding(NSUTF8StringEncoding);
    if bytes > MAX_KEY_CLIPBOARD_BYTES {
        return Err(ClipboardRefusal::TooLong);
    }
    if bytes == 0 {
        return Err(ClipboardRefusal::Invalid);
    }
    let mut buffer = Zeroizing::new(vec![0u8; bytes + 1]);
    let start = NonNull::from(buffer.as_mut_slice()).cast();
    // SAFETY: `start` is writable for `buffer.len()` bytes, the most the call may write.
    let copied =
        unsafe { text.getCString_maxLength_encoding(start, buffer.len(), NSUTF8StringEncoding) };
    if !copied || buffer.iter().position(|&byte| byte == 0) != Some(bytes) {
        return Err(ClipboardRefusal::Invalid);
    }
    let utf8 = std::str::from_utf8(&buffer[..bytes]).map_err(|_| ClipboardRefusal::Invalid)?;
    let mut text = Zeroizing::new(String::with_capacity(bytes));
    text.push_str(utf8);
    Ok(text)
}

#[cfg(test)]
mod tests;
