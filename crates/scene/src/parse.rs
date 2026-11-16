//! Strict JSON decoding without propagating parser diagnostic text.

use crate::{
    ErrorCode, FieldPath, Op, Record, StepError,
    limits::MAX_RECORD_BYTES,
    record::{Surface, Target, Version},
};

/// Parses the closed 2D draw-tool surface, excluding even null solid-only fields.
/// # Errors
/// Returns the first typed decoding failure or an unsupported surface or 3D field.
pub fn parse_draw_record(bytes: &[u8]) -> Result<Record, StepError> {
    parse_draw(bytes, None)
}

/// [`parse_draw_record`] with the screen surface and box targets admitted, as `draw` will parse
/// once the screen surface is offered. A screen scene's steps are checked at admission.
/// # Errors
/// As [`parse_draw_record`]; a model-written display is `Unsupported`.
pub fn parse_screen_draw_record(bytes: &[u8]) -> Result<Record, StepError> {
    parse_draw(bytes, Some(Surface::Screen))
}

fn parse_draw(bytes: &[u8], extra: Option<Surface>) -> Result<Record, StepError> {
    let record = match parse_record(bytes) {
        Err(error)
            if error.code == ErrorCode::MissingField
                && matches!(error.path.as_str(), "v" | "surface") =>
        {
            parse_record(&draw_defaults(bytes)?)?
        }
        record => record?,
    };
    if let Record::Begin(begin) = &record {
        let field = if begin.display.is_some() {
            Some("display")
        } else if begin.surface == Surface::Overlay || extra == Some(begin.surface) {
            None
        } else {
            Some("surface")
        };
        if let Some(field) = field {
            return Err(StepError {
                step: None,
                path: FieldPath::default().field(field),
                code: ErrorCode::Unsupported,
            });
        }
    }
    if let Record::Step(step) = &record {
        crate::refuse_three_dimensional(step)?;
        let boxed = |op: &Op| match op {
            Op::Highlight(mark) => matches!(mark.target, Target::Box(_)),
            Op::Callout(label) => matches!(label.target, Target::Box(_)),
            _ => false,
        };
        if let Some(index) = step.put.iter().position(boxed).filter(|_| extra.is_none()) {
            return Err(StepError::in_step(
                &step.id,
                FieldPath::default()
                    .field("put")
                    .index(index)
                    .field("target"),
                ErrorCode::Unsupported,
            ));
        }
        if !step.set.is_empty() {
            let raw: serde_json::Value =
                serde_json::from_slice(bytes).map_err(|_| StepError::new(ErrorCode::Malformed))?;
            if let Some(index) = raw["step"]["set"]
                .as_array()
                .and_then(|set| set.iter().position(|change| change.get("info").is_some()))
            {
                return Err(StepError::in_step(
                    &step.id,
                    FieldPath::default().field("set").index(index).field("info"),
                    ErrorCode::Unsupported,
                ));
            }
        }
    }
    Ok(record)
}

/// The draw tool supplies its only version and surface when a model's begin omits them.
fn draw_defaults(bytes: &[u8]) -> Result<Vec<u8>, StepError> {
    let mut record: DrawBeginEnvelope = decode(bytes)?;
    let begin = record
        .begin
        .as_object_mut()
        .ok_or_else(|| StepError::new(ErrorCode::Malformed))?;
    begin
        .entry("v")
        .or_insert_with(|| u32::from(Version::V1).into());
    begin.entry("surface").or_insert_with(|| "overlay".into());
    serde_json::to_vec(&record).map_err(|_| StepError::new(ErrorCode::Malformed))
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct DrawBeginEnvelope {
    begin: serde_json::Value,
}

/// Parses one closed record, never echoing drawing content in diagnostics.
/// # Errors
/// Returns a bounded path and code for invalid syntax, fields or values.
pub fn parse_record(bytes: &[u8]) -> Result<Record, StepError> {
    decode(bytes)
}

fn decode<'de, T: serde::Deserialize<'de>>(bytes: &'de [u8]) -> Result<T, StepError> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(StepError::new(ErrorCode::TooLong));
    }
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let record = serde_path_to_error::deserialize(&mut decoder).map_err(|error| {
        let mut path = FieldPath::from_decode(error.path());
        let code = ErrorCode::from_decode(&error.inner().to_string(), &mut path);
        StepError {
            step: None,
            path,
            code,
        }
    })?;
    decoder
        .end()
        .map_err(|_| StepError::new(ErrorCode::Malformed))?;
    Ok(record)
}
