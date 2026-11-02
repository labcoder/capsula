//! Strict scene v1 records shared by the brain and shell.
//! JSON tool arguments and binary messages validate through the same types.

pub mod budget;
pub mod error;
pub mod id;
pub mod limits;
pub mod parse;
pub mod record;
pub mod state;
pub mod text;
pub mod tokenize;

pub use budget::GlobalBudget;
pub use error::{Budget, ErrorCode, FieldPath, StepError};
pub use id::{Handle, Ms, WireId};
pub use parse::parse_record;
pub use record::{Begin, End, Op, Record, Step};
pub use state::{
    Admission, Admitted, CallState, Committed, SceneEvent, SceneState, StaleAdmission, StopCut,
    StopOutcome,
};
pub use text::{Finite, Source, Text};
pub use tokenize::{ClosedRecord, TokenError, Tokenizer};
