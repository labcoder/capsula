//! Content-free validation diagnostics.
use crate::id::WireId;
use std::fmt;

/// The work whose admission limit was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// Steps budget.
    Steps,
    /// `LiveObjects` budget.
    LiveObjects,
    /// `DiagramNodes` budget.
    DiagramNodes,
    /// `DiagramEdges` budget.
    DiagramEdges,
    /// `TableCells` budget.
    TableCells,
    /// `ChartMarks` budget.
    ChartMarks,
    /// `PathPoints` budget.
    PathPoints,
    /// `PathSegments` budget.
    PathSegments,
    /// Tracks budget.
    Tracks,
    /// Keys budget.
    Keys,
    /// Depth budget.
    Depth,
    /// `VisibleScenes` budget.
    VisibleScenes,
    /// Residency budget.
    Residency,
}

/// A stable diagnostic code; never contains drawing content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ErrorCode {
    /// Malformed.
    #[error("scene.Malformed")]
    Malformed,
    /// Unknown Field.
    #[error("scene.UnknownField")]
    UnknownField,
    /// Unknown Op.
    #[error("scene.UnknownOp")]
    UnknownOp,
    /// Duplicate Key.
    #[error("scene.DuplicateKey")]
    DuplicateKey,
    /// A required schema field is absent.
    #[error("scene.MissingField")]
    MissingField,
    /// Not Finite.
    #[error("scene.NotFinite")]
    NotFinite,
    /// Too Long.
    #[error("scene.TooLong")]
    TooLong,
    /// Out Of Range.
    #[error("scene.OutOfRange")]
    OutOfRange,
    /// Wrong Version.
    #[error("scene.WrongVersion")]
    WrongVersion,
    /// Before Begin.
    #[error("scene.BeforeBegin")]
    BeforeBegin,
    /// After End.
    #[error("scene.AfterEnd")]
    AfterEnd,
    /// Unknown Ref.
    #[error("scene.UnknownRef")]
    UnknownRef,
    /// Forward Ref.
    #[error("scene.ForwardRef")]
    ForwardRef,
    /// Id Reused.
    #[error("scene.IdReused")]
    IdReused,
    /// Kind Changed.
    #[error("scene.KindChanged")]
    KindChanged,
    /// Orphans.
    #[error("scene.Orphans")]
    Orphans,
    /// Duplicate Step Conflict.
    #[error("scene.DuplicateStepConflict")]
    DuplicateStepConflict,
    /// Property Not Owned.
    #[error("scene.PropertyNotOwned")]
    PropertyNotOwned,
    /// Too Deep.
    #[error("scene.TooDeep")]
    TooDeep,
    /// Transform Overflow.
    #[error("scene.TransformOverflow")]
    TransformOverflow,
    /// Unsupported.
    #[error("scene.Unsupported")]
    Unsupported,
    /// Scene Cleared.
    #[error("scene.SceneCleared")]
    SceneCleared,
    /// Header Mismatch.
    #[error("scene.HeaderMismatch")]
    HeaderMismatch,
    /// Expanded work exceeds a budget.
    #[error("scene.OverBudget")]
    OverBudget(Budget),
    /// References form a cycle that cannot be laid out.
    #[error("scene.ReferenceCycle")]
    ReferenceCycle,
}
impl ErrorCode {
    pub(crate) fn from_decode(message: &str, path: &mut FieldPath) -> Self {
        if message.starts_with("scene.NotFinite") {
            return Self::NotFinite;
        }
        if message.starts_with("scene.TooLong") {
            return Self::TooLong;
        }
        if message.starts_with("scene.OutOfRange") {
            return Self::OutOfRange;
        }
        if message.starts_with("scene.WrongVersion") {
            return Self::WrongVersion;
        }
        if message.starts_with("unknown field") {
            Self::UnknownField
        } else if message.starts_with("unknown variant") {
            if path.is_op_tag() {
                Self::UnknownOp
            } else {
                Self::OutOfRange
            }
        } else if message.starts_with("duplicate field") {
            Self::DuplicateKey
        } else if let Some(missing) = message.strip_prefix("missing field `") {
            if let Some((field, _)) = missing.split_once('`')
                && known_field(field)
            {
                path.push_field(field);
            }
            Self::MissingField
        } else if message.starts_with("invalid value") || message.starts_with("number out of range")
        {
            Self::OutOfRange
        } else {
            Self::Malformed
        }
    }
}

/// A bounded path containing only schema fields and array indices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldPath(String);
impl FieldPath {
    /// Returns the sanitized structural path.
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub(crate) fn field(mut self, field: &'static str) -> Self {
        assert!(
            known_field(field),
            "semantic paths use schema field literals"
        );
        self.push_field(field);
        self
    }
    pub(crate) fn index(mut self, index: usize) -> Self {
        use std::fmt::Write;
        write!(self.0, "[{index}]").expect("writing to a string cannot fail");
        self
    }
    fn is_op_tag(&self) -> bool {
        self.0.is_empty()
            || self
                .0
                .strip_prefix("put[")
                .and_then(|s| s.strip_suffix(']'))
                .is_some_and(|index| !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()))
    }
    fn push_field(&mut self, field: &str) {
        if !self.0.is_empty() {
            self.0.push('.');
        }
        self.0.push_str(field);
    }
    pub(crate) fn from_decode(path: &serde_path_to_error::Path) -> Self {
        use serde_path_to_error::Segment;
        let mut out = String::new();
        for segment in path {
            if out.len() >= 240 {
                break;
            }
            match segment {
                Segment::Enum { variant }
                    if out.is_empty() && matches!(variant.as_str(), "step" | "begin" | "end") => {}
                Segment::Seq { index } => {
                    use std::fmt::Write;
                    write!(out, "[{index}]").expect("writing to a string cannot fail");
                }
                Segment::Map { key } if known_field(key) => {
                    if !out.is_empty() {
                        out.push('.');
                    }
                    out.push_str(key);
                }
                Segment::Enum { variant } if known_field(variant) => {
                    if !out.is_empty() {
                        out.push('.');
                    }
                    out.push_str(variant);
                }
                _ => break,
            }
        }
        Self(out)
    }
}
impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            f.write_str("$")
        } else {
            f.write_str(&self.0)
        }
    }
}

fn known_field(key: &str) -> bool {
    matches!(
        key,
        "begin"
            | "revision"
            | "stop"
            | "activated"
            | "revealed"
            | "step"
            | "end"
            | "v"
            | "scene"
            | "surface"
            | "size"
            | "title"
            | "alt"
            | "id"
            | "say"
            | "put"
            | "set"
            | "remove"
            | "start"
            | "after"
            | "ms"
            | "line"
            | "arrow"
            | "path"
            | "shape"
            | "text"
            | "callout"
            | "highlight"
            | "diagram"
            | "node"
            | "edge"
            | "table"
            | "row"
            | "chart"
            | "series"
            | "anim"
            | "mermaid"
            | "target"
            | "label"
            | "color"
            | "cells"
            | "emphasis"
            | "data"
            | "in"
            | "delay"
            | "at_word"
            | "enter"
            | "dur"
            | "opacity"
            | "from"
            | "to"
            | "via"
            | "width"
            | "dash"
            | "head"
            | "bend"
            | "d"
            | "at"
            | "scale"
            | "fill"
            | "kind"
            | "box"
            | "r"
            | "points"
            | "radius"
            | "bold"
            | "align"
            | "max_width"
            | "side"
            | "style"
            | "pad"
            | "dir"
            | "sub"
            | "flow"
            | "cols"
            | "categories"
            | "x"
            | "y"
            | "stacked"
            | "name"
            | "min"
            | "max"
            | "unit"
            | "track"
            | "keys"
            | "ease"
            | "repeat"
            | "yoyo"
            | "src"
            | "ref"
            | "rotate"
            | "draw"
            | "pos"
    )
}

/// A validation failure with a private-safe diagnostic surface.
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
#[error("{path}: {code}")]
pub struct StepError {
    /// Model id, returned only to that model by `tool_text`.
    pub step: Option<WireId>,
    /// Sanitized field path.
    pub path: FieldPath,
    /// Stable content-free code.
    pub code: ErrorCode,
}
impl StepError {
    pub(crate) fn in_step(step: &WireId, path: FieldPath, code: ErrorCode) -> Self {
        Self {
            step: Some(step.clone()),
            path,
            code,
        }
    }
    /// Builds a root-level failure.
    pub fn new(code: ErrorCode) -> Self {
        Self {
            step: None,
            path: FieldPath::default(),
            code,
        }
    }
    /// Formats a model reply, including its own validated step id, never field values.
    pub fn tool_text(&self) -> String {
        match &self.step {
            Some(step) => format!("step {}: {self}", step.as_str()),
            None => self.to_string(),
        }
    }
}
impl fmt::Debug for StepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StepError")
            .field("path", &self.path)
            .field("code", &self.code)
            .finish_non_exhaustive()
    }
}
