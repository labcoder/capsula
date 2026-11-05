//! Perching Kumo on another app's title bar: the caption a drop counts on, and where the perch
//! sits. Pure geometry; the window reads are in `platform::host`.
#![expect(dead_code, reason = "C2b-2's carry and C2b-3's perch are the callers")]

mod caption;
mod place;
