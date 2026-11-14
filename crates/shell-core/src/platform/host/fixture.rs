//! A window title no test output may hold, and the check for it.

use super::Label;

pub(crate) const SENTINEL: &str = "SENTINEL-TITLE-7f3a";

pub fn sentinel() -> Label {
    Label::new(&SENTINEL.encode_utf16().collect::<Vec<_>>())
}

/// Fails when `text` holds the sentinel as text, as UTF-8 bytes or as UTF-16 units.
pub fn assert_no_sentinel(text: &str) {
    assert!(!text.contains(SENTINEL), "{text}");
    let units: Vec<u16> = SENTINEL.encode_utf16().collect();
    let bytes: Vec<u8> = units.iter().flat_map(|unit| unit.to_le_bytes()).collect();
    assert!(
        !text.as_bytes().windows(bytes.len()).any(|w| w == bytes),
        "{text}"
    );
    let written: Vec<u16> = text.encode_utf16().collect();
    assert!(!written.windows(units.len()).any(|w| w == units), "{text}");
    let list = |values: Vec<String>| values.join(", ");
    assert!(
        !text.contains(&list(units.iter().map(u16::to_string).collect())),
        "{text}"
    );
    assert!(
        !text.contains(&list(SENTINEL.bytes().map(|b| b.to_string()).collect())),
        "{text}"
    );
}
