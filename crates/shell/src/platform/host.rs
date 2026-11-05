//! What the perch reads about another app's top-level window, as plain data. The Windows reads
//! live in `windows/host.rs`; the caption rules that use them live in `presence/perch/`.

use std::fmt;

/// A rectangle in physical screen pixels, half-open: `[left, right) × [top, bottom)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PhysicalRect {
    pub(crate) left: i32,
    pub(crate) top: i32,
    pub(crate) right: i32,
    pub(crate) bottom: i32,
}

impl PhysicalRect {
    /// Nonempty and ordered.
    pub(crate) fn usable(self) -> bool {
        self.left < self.right && self.top < self.bottom
    }

    pub(crate) fn contains(self, inner: Self) -> bool {
        inner.left >= self.left
            && inner.top >= self.top
            && inner.right <= self.right
            && inner.bottom <= self.bottom
    }

    /// The overlap, which may be unusable when there's none.
    pub(crate) fn intersect(self, other: Self) -> Self {
        Self {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        }
    }

    pub(crate) fn offset(self, [x, y]: [i32; 2]) -> Self {
        Self {
            left: self.left + x,
            top: self.top + y,
            right: self.right + x,
            bottom: self.bottom + y,
        }
    }
}

/// Which window a target is: the handle, process and thread. A handle can be reused on the same
/// thread, so only a watch proves the window is still the same one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HostId {
    pub(crate) window: usize,
    pub(crate) process: u32,
    pub(crate) thread: u32,
}

/// One watch's generation, minted by Presence and never reused in the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct WatchGen(pub(crate) u64);

/// What a watched host did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostChange {
    /// It moved or resized.
    Moved,
    /// It was destroyed, hidden, minimized or cloaked.
    Gone,
}

/// The raw caption reads for one window, in physical pixels unless noted. The rules that turn
/// them into a caption are in `presence/perch/caption.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Facts {
    /// The calling thread really is per-monitor aware v2, so every rectangle is physical.
    pub(crate) per_monitor_v2: bool,
    /// `(style & WS_CAPTION) == WS_CAPTION`.
    pub(crate) captioned: bool,
    /// The raw window rectangle.
    pub(crate) bounds: PhysicalRect,
    /// The visible frame.
    pub(crate) frame: PhysicalRect,
    /// The work area of the frame's monitor.
    pub(crate) work_area: PhysicalRect,
    /// The DPI the host lays itself out at.
    pub(crate) layout_dpi: u32,
    /// The effective DPI of the frame's monitor.
    pub(crate) monitor_dpi: u32,
    /// The visible title bar, as reported, in screen coordinates.
    pub(crate) title_bar: Option<PhysicalRect>,
    /// The caption buttons as reported, relative to `bounds`' top-left, in unpinned units.
    pub(crate) buttons: Option<PhysicalRect>,
    /// The standard top inset for the host's style, in host units; a resizable window's when
    /// it has no caption.
    pub(crate) top_inset: i32,
    /// One standard caption button's width, in host units.
    pub(crate) button_width: i32,
}

/// The window under the pointer, as read. Its `Debug` never prints the title.
#[derive(Clone, Copy)]
pub(crate) struct HostFacts {
    pub(crate) id: HostId,
    pub(crate) caption: Facts,
    pub(crate) label: Label,
}

impl fmt::Debug for HostFacts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostFacts")
            .field("id", &self.id)
            .field("caption", &self.caption)
            .field("label", &self.label)
            .finish()
    }
}

/// UTF-16 units a label keeps.
pub(crate) const LABEL_UNITS: usize = 64;

/// Another app's window title, screen text. It's kept inline, so reading one allocates
/// nothing, and its `Debug` prints only its length. It goes to the drop label and nowhere else.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Label {
    units: [u16; LABEL_UNITS],
    len: u8,
}

impl Label {
    /// The first [`LABEL_UNITS`] units of `title`, without a trailing half of a surrogate pair.
    pub(crate) fn new(title: &[u16]) -> Self {
        let mut len = title.len().min(LABEL_UNITS);
        if len > 0 && (0xD800..0xDC00).contains(&title[len - 1]) {
            len -= 1;
        }
        let mut units = [0; LABEL_UNITS];
        units[..len].copy_from_slice(&title[..len]);
        Self {
            units,
            len: u8::try_from(len).unwrap_or(0),
        }
    }

    pub(crate) fn units(&self) -> &[u16] {
        &self.units[..usize::from(self.len)]
    }
}

impl fmt::Debug for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Label({} units)", self.len)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{Facts, HostFacts, HostId, LABEL_UNITS, Label, PhysicalRect};

    pub(crate) const SENTINEL: &str = "SENTINEL-TITLE-7f3a";

    pub(crate) fn sentinel() -> Label {
        Label::new(&SENTINEL.encode_utf16().collect::<Vec<_>>())
    }

    /// Fails when `text` holds the sentinel as text, as UTF-8 bytes or as UTF-16 units.
    pub(crate) fn assert_no_sentinel(text: &str) {
        assert!(!text.contains(SENTINEL), "{text}");
        let units: Vec<u16> = SENTINEL.encode_utf16().collect();
        let bytes: Vec<u8> = units.iter().flat_map(|unit| unit.to_le_bytes()).collect();
        assert!(!text.as_bytes().windows(bytes.len()).any(|w| w == bytes));
        let written: Vec<u16> = text.encode_utf16().collect();
        assert!(!written.windows(units.len()).any(|w| w == units));
    }

    #[test]
    fn a_label_keeps_at_most_its_units_and_never_half_a_pair() {
        let long: Vec<u16> = "a".repeat(70).encode_utf16().collect();
        assert_eq!(Label::new(&long).units(), &long[..LABEL_UNITS]);
        let mut cut: Vec<u16> = "a".repeat(63).encode_utf16().collect();
        cut.extend("😀".encode_utf16());
        assert_eq!(cut.len(), 65);
        assert_eq!(
            Label::new(&cut).units(),
            &cut[..63],
            "the high half is dropped"
        );
        let mut whole: Vec<u16> = "a".repeat(62).encode_utf16().collect();
        whole.extend("😀".encode_utf16());
        assert_eq!(
            Label::new(&whole).units(),
            &whole[..],
            "a whole pair at the end stays"
        );
        assert!(Label::new(&[]).units().is_empty());
    }

    #[test]
    fn no_title_reaches_debug() {
        let facts = HostFacts {
            id: HostId {
                window: 1,
                process: 2,
                thread: 3,
            },
            caption: Facts {
                per_monitor_v2: true,
                captioned: true,
                bounds: PhysicalRect::default(),
                frame: PhysicalRect::default(),
                work_area: PhysicalRect::default(),
                layout_dpi: 96,
                monitor_dpi: 96,
                title_bar: None,
                buttons: None,
                top_inset: 31,
                button_width: 36,
            },
            label: sentinel(),
        };
        let text = format!("{facts:?} {:?}", facts.label);
        assert!(text.contains("Label(19 units)"), "{text}");
        assert_no_sentinel(&text);
    }
}
