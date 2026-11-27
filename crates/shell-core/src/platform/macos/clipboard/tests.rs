use std::time::Duration;

use objc2::msg_send;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::NSPasteboardWriting;
use objc2_foundation::{NSArray, NSData};

use super::*;

const MAX: usize = MAX_KEY_CLIPBOARD_BYTES;

fn copied(text: &str) -> KeyClipboardResult {
    copy(&NSString::from_str(text))
}

#[test]
fn the_copy_takes_exactly_the_limit_and_refuses_one_byte_more() {
    let limit = "k".repeat(MAX);
    assert_eq!(copied(&limit).expect("at the limit").as_str(), limit);
    assert_eq!(copied(&"k".repeat(MAX + 1)), Err(ClipboardRefusal::TooLong));
}

#[test]
fn a_multibyte_character_past_the_byte_limit_is_too_long() {
    let text = format!("{}é", "k".repeat(MAX - 1));
    assert_eq!(text.encode_utf16().count(), MAX);
    assert_eq!(copied(&text), Err(ClipboardRefusal::TooLong));
    let fits = format!("{}é", "k".repeat(MAX - 2));
    assert_eq!(copied(&fits).expect("fits").as_str(), fits);
}

#[test]
fn an_empty_or_nul_carrying_string_is_invalid() {
    assert_eq!(copied(""), Err(ClipboardRefusal::Invalid));
    assert_eq!(copied("sk-ant\0tail"), Err(ClipboardRefusal::Invalid));
}

/// A pasteboard nobody else sees, released when the test ends. Only the general pasteboard
/// asks before a read, so this never shows an alert or touches the user's clipboard.
struct Private(Retained<NSPasteboard>);

impl Private {
    fn new() -> Self {
        let board = NSPasteboard::pasteboardWithUniqueName();
        board.clearContents();
        Self(board)
    }

    fn holding(text: &str) -> Self {
        let private = Self::new();
        // SAFETY: AppKit's string type constant, which never changes while the process runs.
        let kind = unsafe { NSPasteboardTypeString };
        assert!(private.0.setString_forType(&NSString::from_str(text), kind));
        private
    }

    fn read(&self, expected: isize, deadline: Instant) -> KeyClipboardResult {
        read_from(&self.0, expected, deadline, || true)
    }
}

impl Drop for Private {
    fn drop(&mut self) {
        // SAFETY: `releaseGlobally` takes no arguments and returns nothing; the pasteboard is
        // this test's own and is never used after this.
        let () = unsafe { msg_send![&*self.0, releaseGlobally] };
    }
}

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

#[test]
#[ignore = "native: uses a private macOS pasteboard"]
fn native_a_private_pasteboard_reads_its_one_string_exactly() {
    let private = Private::holding("  sk-ant-api03-fixture  ");
    let count = private.0.changeCount();
    assert_eq!(
        private.read(count, soon()).expect("one string").as_str(),
        "  sk-ant-api03-fixture  "
    );
}

#[test]
#[ignore = "native: uses a private macOS pasteboard"]
fn native_a_change_after_capture_is_refused() {
    let private = Private::holding("sk-ant-api03-first");
    let count = private.0.changeCount();
    private.0.clearContents();
    // SAFETY: AppKit's string type constant, which never changes while the process runs.
    let kind = unsafe { NSPasteboardTypeString };
    assert!(
        private
            .0
            .setString_forType(&NSString::from_str("sk-ant-api03-second"), kind)
    );
    assert_eq!(private.read(count, soon()), Err(ClipboardRefusal::Changed));
}

#[test]
#[ignore = "native: uses a private macOS pasteboard"]
fn native_two_items_or_no_string_are_invalid() {
    let two = Private::new();
    let items = NSArray::from_retained_slice(&[
        ProtocolObject::<dyn NSPasteboardWriting>::from_retained(NSString::from_str("a")),
        ProtocolObject::<dyn NSPasteboardWriting>::from_retained(NSString::from_str("b")),
    ]);
    assert!(two.0.writeObjects(&items));
    assert_eq!(
        two.read(two.0.changeCount(), soon()),
        Err(ClipboardRefusal::Invalid)
    );

    let other = Private::new();
    let kind = NSString::from_str("org.kumochi.test.other");
    assert!(
        other
            .0
            .setData_forType(Some(&NSData::with_bytes(b"sk-ant")), &kind)
    );
    assert_eq!(
        other.read(other.0.changeCount(), soon()),
        Err(ClipboardRefusal::Invalid)
    );
}

#[test]
#[ignore = "native: uses a private macOS pasteboard"]
fn native_an_ended_deadline_or_stale_session_reads_nothing() {
    let private = Private::holding("sk-ant-api03-fixture");
    let count = private.0.changeCount();
    assert_eq!(
        private.read(count, Instant::now()),
        Err(ClipboardRefusal::Unavailable)
    );
    assert_eq!(
        read_from(&private.0, count, soon(), || false),
        Err(ClipboardRefusal::Unavailable)
    );
}
