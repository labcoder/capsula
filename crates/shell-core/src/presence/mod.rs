//! The models behind the presence: the drag owner, the capture exclusion's lease, Kumo, the frame
//! pacing, the dock line's machine, the perch rules, the pull, the layout and the thread selection.

pub mod drag;
#[expect(
    clippy::as_conversions,
    reason = "eight fixed semantic slot discriminants index the fixed receipt array"
)]
pub mod exclusion {
    pub mod machine;
}
pub mod frame_rate;
pub mod geometry;
pub mod kumo;
pub mod line {
    pub mod machine;
}
pub mod perch;
pub mod pulling;
pub mod thread_selection;
