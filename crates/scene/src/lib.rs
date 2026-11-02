//! Strict scene v1 records shared by the brain and shell.
//! JSON tool arguments and binary messages validate through the same types.

pub mod error;
pub mod id;
pub mod limits;
pub mod parse;
pub mod record;
pub mod text;

pub use error::{Budget, ErrorCode, FieldPath, StepError};
pub use id::{Handle, Ms, WireId};
pub use parse::parse_record;
pub use record::{Begin, End, Op, Record, Step};
pub use text::{Finite, Source, Text};
