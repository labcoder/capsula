//! Strict JSON decoding without propagating parser diagnostic text.

use crate::{ErrorCode, FieldPath, Record, StepError, limits::MAX_RECORD_BYTES};

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
