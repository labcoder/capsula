//! Perching Kumo on another app's title bar: the caption a drop counts on, and where the perch
//! sits. Pure geometry; the window reads are in `platform::host`.
#![expect(dead_code, reason = "C2b-2's carry and C2b-3's perch are the callers")]

mod caption;
mod place;
mod target;

pub(super) use caption::{WindowTarget, drop_target};
pub(super) use place::{Extents, Placement};
#[cfg(test)]
pub(super) use target::tests as target_fixtures;
pub(super) use target::{Check, Drop, Targeting};
