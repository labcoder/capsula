//! Reads only the text needed for an explicit Anthropic key paste.

use std::time::Instant;

use zeroize::Zeroizing;

pub(crate) const MAX_KEY_CLIPBOARD_BYTES: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ClipboardRefusal {
    #[error("this platform has no bounded key clipboard reader")]
    Unsupported,
    #[error("the clipboard is unavailable")]
    Unavailable,
    #[error("the clipboard changed")]
    Changed,
    #[error("expected one valid clipboard text value")]
    Invalid,
    #[error("the clipboard text exceeds the key paste limit")]
    TooLong,
}

pub type KeyClipboardResult = Result<Zeroizing<String>, ClipboardRefusal>;

pub struct KeyClipboardRead {
    #[cfg(windows)]
    sequence: u32,
}

impl KeyClipboardRead {
    #[cfg(windows)]
    pub fn capture() -> Result<Self, ClipboardRefusal> {
        let sequence = super::windows::clipboard::sequence();
        if sequence == 0 {
            return Err(ClipboardRefusal::Unavailable);
        }
        Ok(Self { sequence })
    }

    #[cfg(not(windows))]
    pub fn capture() -> Result<Self, ClipboardRefusal> {
        Err(ClipboardRefusal::Unsupported)
    }

    #[cfg(windows)]
    pub fn read(self, deadline: Instant, is_current: impl Fn() -> bool) -> KeyClipboardResult {
        super::windows::clipboard::read(self.sequence, deadline, is_current)
    }

    #[cfg(not(windows))]
    pub fn read(self, _: Instant, _: impl Fn() -> bool) -> KeyClipboardResult {
        let Self {} = self;
        Err(ClipboardRefusal::Unsupported)
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn unsupported_platform_refuses_a_key_clipboard_read() {
        assert_eq!(
            KeyClipboardRead::capture().err(),
            Some(ClipboardRefusal::Unsupported)
        );
    }
}
