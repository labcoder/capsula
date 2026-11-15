//! The shell's side of the drawing presenter: the core's presenter, and the text measure that
//! needs GPUI.

mod measure;

pub(crate) use kumochi_shell_core::presenter::{
    Broken, Completion, Effect, Frames, Presenter, Sample, SceneCache, Stage, overlay_input,
};
pub(crate) use measure::Measure;

#[cfg(test)]
pub(crate) mod tests;
