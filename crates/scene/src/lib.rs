//! Strict scene v1 records shared by the brain and shell.
//! JSON tool arguments and binary messages validate through the same types.

pub mod budget;
pub mod deferred;
pub mod display;
pub mod error;
pub mod geom;
pub mod id;
pub mod layout;
pub mod limits;
mod mermaid;
pub mod motion;
pub mod parse;
mod path;
pub mod record;
mod schema;
pub mod screen;
pub mod state;
pub mod text;
pub mod theme;
pub mod three;
pub mod tokenize;

pub use budget::GlobalBudget;
pub use error::{Budget, ErrorCode, FieldPath, StepError};
pub use id::{Handle, Ms, WireId};
pub use motion::{Frame, FrameItem, PreparedActivation, SceneView, ViewFrame};
pub use parse::{
    check_overlay_draw_record, parse_draw_record, parse_record, parse_screen_draw_record,
};
pub use record::{Begin, End, Op, Others, Record, Step};
pub use schema::{draw_schema, screen_draw_schema};
pub use state::{
    Admission, Admitted, BindingError, CallState, Committed, DetachedCandidate, Part, Preparation,
    ReplacementAdmission, ReplacementDisposition, SceneBinding, SceneEvent, SceneRead, SceneState,
    StaleAdmission, StopCut, StopOutcome, refuse_three_dimensional,
};
pub use text::{Finite, Source, Text};
pub use tokenize::{ClosedRecord, TokenError, Tokenizer};
