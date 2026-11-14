//! Perching Kumo on another app's title bar: the caption a drop counts on, and where the perch
//! sits. Pure geometry; the window reads are in `platform::host`.

mod caption;
mod place;
mod target;

pub use caption::{WindowTarget, drop_target};
pub use place::{Extents, Placement};
#[cfg(any(test, feature = "test-support"))]
pub use target::fixture as target_fixtures;
pub use target::{Check, Drop, Targeting};
