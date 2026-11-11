//! Strict JSON decoding without propagating parser diagnostic text.

use crate::{ErrorCode, FieldPath, Record, StepError, limits::MAX_RECORD_BYTES, record::Surface};

/// Parses the closed 2D draw-tool surface, excluding even null solid-only fields.
/// # Errors
/// Returns the first typed decoding failure or an unsupported surface or 3D field.
pub fn parse_draw_record(bytes: &[u8]) -> Result<Record, StepError> {
    let record = parse_record(bytes)?;
    if let Record::Begin(begin) = &record
        && begin.surface != Surface::Overlay
    {
        return Err(StepError {
            step: None,
            path: FieldPath::default().field("surface"),
            code: ErrorCode::Unsupported,
        });
    }
    if let Record::Step(step) = &record {
        crate::refuse_three_dimensional(step)?;
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

/// Parses one closed record, never echoing drawing content in diagnostics.
/// # Errors
/// Returns a bounded path and code for invalid syntax, fields or values.
pub fn parse_record(bytes: &[u8]) -> Result<Record, StepError> {
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
