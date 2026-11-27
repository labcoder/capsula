//! Normalized, non-printing shortcut settings. No platform key codes or key events.

use std::fmt;

use serde::{Deserialize, Serialize};

const LEFT_CTRL: u8 = 1 << 0;
const CTRL: u8 = LEFT_CTRL | (1 << 1);
const LEFT_ALT: u8 = 1 << 2;
const SHIFT: u8 = (1 << 4) | (1 << 5);
const WIN: u8 = (1 << 6) | (1 << 7);
const ALLOWED: u8 = CTRL | LEFT_ALT | SHIFT | WIN;

/// A checked set of sided modifiers and at most one function key.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ShortcutParts")]
pub struct ShortcutChord {
    modifiers: u8,
    function: Option<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShortcutParts {
    modifiers: u8,
    function: Option<u8>,
}

/// The candidate does not meet the portable shortcut rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unsupported shortcut chord")]
pub struct InvalidShortcutChord;

impl ShortcutChord {
    /// Left Ctrl + left Alt. A stored choice does not change the live binding yet.
    pub const DEFAULT: Self = Self {
        modifiers: LEFT_CTRL | LEFT_ALT,
        function: None,
    };

    /// Checks normalized parts. Modifier bits 0–7 are left/right Ctrl, left/right Alt,
    /// left/right Shift, left/right Win. A function key is its number (F1–F24).
    /// Ctrl with Shift is refused because its Escape projection conflicts with Task Manager.
    ///
    /// # Errors
    /// [`InvalidShortcutChord`] for a disallowed modifier, key count, anchor or reserved chord.
    pub fn new(modifiers: u8, function: Option<u8>) -> Result<Self, InvalidShortcutChord> {
        if modifiers & !ALLOWED != 0 {
            return Err(InvalidShortcutChord);
        }
        let ctrl = modifiers & CTRL != 0;
        let alt = modifiers & LEFT_ALT != 0;
        let shift = modifiers & SHIFT != 0;
        let win = modifiers & WIN != 0;
        let count = modifiers.count_ones();
        let allowed = match function {
            None => {
                let families = [ctrl, alt, shift, win]
                    .into_iter()
                    .filter(|&held| held)
                    .count();
                count <= 4 && families >= 2
            }
            Some(key) => {
                (1..=3).contains(&count) && (1..=24).contains(&key) && (ctrl || alt || win)
            }
        };
        let reserved = (ctrl && shift)
            || (shift && win && function == Some(23))
            || ((alt || (win && ctrl)) && function == Some(4));
        if !allowed || reserved {
            return Err(InvalidShortcutChord);
        }
        Ok(Self {
            modifiers,
            function,
        })
    }

    /// The normalized sided modifier bits.
    #[must_use]
    pub fn modifiers(self) -> u8 {
        self.modifiers
    }

    /// The function key's number, if present.
    #[must_use]
    pub fn function(self) -> Option<u8> {
        self.function
    }
}

impl TryFrom<ShortcutParts> for ShortcutChord {
    type Error = InvalidShortcutChord;

    fn try_from(parts: ShortcutParts) -> Result<Self, Self::Error> {
        Self::new(parts.modifiers, parts.function)
    }
}

impl fmt::Debug for ShortcutChord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ShortcutChord")
    }
}

/// The stored shortcut's status, whether or not custom recognition is available.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShortcutView {
    /// No override is stored.
    #[default]
    Default,
    /// An override passed storage validation; this does not authorize recognition.
    Custom(ShortcutChord),
    /// The saved value is invalid and was left untouched.
    Invalid,
}

impl ShortcutView {
    /// The stored preference with default fallback, not an armed binding.
    #[must_use]
    pub fn effective(self) -> ShortcutChord {
        match self {
            Self::Custom(chord) => chord,
            Self::Default | Self::Invalid => ShortcutChord::DEFAULT,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InvalidShortcutChord, ShortcutChord};

    #[test]
    fn shortcut_validation_counts_sides_and_families_and_refuses_reserved_chords() {
        let lc = 1 << 0;
        let rc = 1 << 1;
        let la = 1 << 2;
        let ra = 1 << 3;
        let ls = 1 << 4;
        let rs = 1 << 5;
        let lw = 1 << 6;
        let rw = 1 << 7;

        assert_eq!(
            ShortcutChord::new(lc | la, None),
            Ok(ShortcutChord::DEFAULT)
        );
        for (mask, function) in [
            (rs | rw, None),
            (lc | rc | lw | rw, None),
            (rc, Some(1)),
            (rw | ls | rs, Some(24)),
        ] {
            let chord = ShortcutChord::new(mask, function).expect("valid normalized chord");
            assert_eq!((chord.modifiers(), chord.function()), (mask, function));
        }
        for (mask, function) in [
            (0, None),
            (lc, None),
            (lc | rc, None),
            (ls | rs, None),
            (lc | ra, None),
            (lc | rc | ls | rs, None),
            (lc | rc | ls | rs | lw, None),
            (rc | la | rs | rw, None),
            (0, Some(1)),
            (ls | rs, Some(1)),
            (lc | rc | ls | rs, Some(1)),
            (lc, Some(0)),
            (lc, Some(25)),
            (lc | ra, Some(1)),
            (rs | rw, Some(23)),
            (rc | ls | lw, Some(23)),
            (la, Some(4)),
            (rc | la | rs, Some(4)),
            (rc | rw | rs, Some(4)),
        ] {
            assert_eq!(
                ShortcutChord::new(mask, function),
                Err(InvalidShortcutChord)
            );
        }
    }

    #[test]
    fn shortcut_refuses_task_manager_projection_across_sides_and_supersets() {
        for ctrl in [1, 2] {
            for shift in [16, 32] {
                for extra in [0, 4, 64, 128] {
                    assert_eq!(
                        ShortcutChord::new(ctrl | shift | extra, None),
                        Err(InvalidShortcutChord)
                    );
                    assert_eq!(
                        ShortcutChord::new(ctrl | shift | extra, Some(6)),
                        Err(InvalidShortcutChord)
                    );
                }
            }
        }
    }
}
