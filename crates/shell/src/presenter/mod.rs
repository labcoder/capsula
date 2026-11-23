//! The shell's side of the drawing presenter: the core's presenter, and the text measure that
//! needs GPUI.

mod measure;
mod text_profile;

pub(crate) use kumochi_shell_core::presenter::{
    Broken, Completion, Effect, Frames, Presenter, Sample, SceneCache, Stage, overlay_input,
};
pub(crate) use measure::{Measure, drawing_font, resolve_font};
pub(crate) use text_profile::{PairFont, text_profiles};

#[cfg(test)]
pub(crate) mod tests;
