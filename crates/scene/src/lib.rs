//! Strict scene v1 records shared by the brain and shell.
//! JSON tool arguments and binary messages validate through the same types.

pub mod budget;
pub mod display;
pub mod error;
pub mod geom;
pub mod id;
pub mod layout;
pub mod limits;
#[cfg(test)]
mod mermaid;
pub mod motion;
pub mod parse;
mod path;
pub mod record;
pub mod state;
pub mod text;
pub mod theme;
pub mod tokenize;

pub use budget::GlobalBudget;
pub use error::{Budget, ErrorCode, FieldPath, StepError};
pub use id::{Handle, Ms, WireId};
pub use motion::{Frame, FrameItem, SceneView};
pub use parse::parse_record;
pub use record::{Begin, End, Op, Record, Step};
pub use state::{
    Admission, Admitted, CallState, Committed, Preparation, SceneEvent, SceneState, StaleAdmission,
    StopCut, StopOutcome,
};
pub use text::{Finite, Source, Text};
pub use tokenize::{ClosedRecord, TokenError, Tokenizer};
