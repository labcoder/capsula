//! Consumes a validated paste and a brain-issued permit without sending key bytes on the pipe.

use std::fmt;

use kumochi_protocol::constants::{self, CREDENTIAL_SERVICE};
use kumochi_protocol::message::{KeyRejection, KeyStageId, KeyStagePlan, validate_anthropic_key};
use zeroize::Zeroizing;

pub(crate) struct PastedAnthropicKey(Zeroizing<String>);

impl PastedAnthropicKey {
    pub(crate) fn new(text: Zeroizing<String>) -> Result<Self, KeyRejection> {
        validate_anthropic_key(&text)?;
        Ok(Self(text))
    }

    pub(crate) fn len(&self) -> u16 {
        u16::try_from(self.0.trim().len()).expect("validated key length fits u16")
    }
}

impl fmt::Debug for PastedAnthropicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PastedAnthropicKey(***)")
    }
}

#[derive(Debug)]
pub(crate) struct StagePermit(KeyStagePlan);

impl StagePermit {
    pub(crate) fn new(plan: KeyStagePlan) -> Self {
        Self(plan)
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum StageError {
    #[error("invalid staging plan")]
    InvalidPlan,
    #[error("staging length mismatch")]
    LengthMismatch,
    #[error("the credential store is unavailable")]
    Store,
}

#[cfg(any(windows, test))]
pub(crate) trait KeyStageWriter {
    fn write(&self, permit: StagePermit, key: PastedAnthropicKey)
    -> Result<KeyStageId, StageError>;
}

#[cfg(windows)]
pub(crate) fn write_key_stage(
    permit: StagePermit,
    key: PastedAnthropicKey,
) -> Result<KeyStageId, StageError> {
    super::windows::WindowsKeyStageWriter.write(permit, key)
}

#[cfg(not(windows))]
pub(crate) fn write_key_stage(
    permit: StagePermit,
    key: PastedAnthropicKey,
) -> Result<KeyStageId, StageError> {
    write_stage(permit, key, |_, _| Err(StageError::Store))
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "a write attempt consumes both the one-use permit and the pasted key"
)]
pub(crate) fn write_stage(
    permit: StagePermit,
    key: PastedAnthropicKey,
    mut store: impl FnMut(&str, &[u8]) -> Result<(), StageError>,
) -> Result<KeyStageId, StageError> {
    let plan = permit.0;
    let layout = plan.layout().ok_or(StageError::InvalidPlan)?;
    let bytes = key.0.trim().as_bytes();
    if layout.bytes() != bytes.len() {
        return Err(StageError::LengthMismatch);
    }
    let generation = constants::generation_hex(&plan.id.0);
    let mut offset = 0;
    for index in 0..layout.chunks() {
        let end = offset + layout.chunk_len(index);
        let target =
            constants::generation_target(CREDENTIAL_SERVICE, "anthropic", &generation, index);
        store(&target, &bytes[offset..end])?;
        offset = end;
    }
    Ok(plan.id)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[cfg(windows)]
    #[test]
    fn offline_native_header_is_generic_and_local_machine() {
        crate::platform::windows::assert_offline_native_header();
    }

    #[derive(Default)]
    struct FakeWriter {
        writes: RefCell<Vec<(String, Vec<u8>)>>,
        fail_at: Option<usize>,
    }

    impl KeyStageWriter for FakeWriter {
        #[expect(
            clippy::panic_in_result_fn,
            reason = "the fake asserts the write-once contract"
        )]
        fn write(
            &self,
            permit: StagePermit,
            key: PastedAnthropicKey,
        ) -> Result<KeyStageId, StageError> {
            write_stage(permit, key, |target, bytes| {
                let mut writes = self.writes.borrow_mut();
                assert!(writes.iter().all(|(name, _)| name != target));
                writes.push((target.to_owned(), bytes.to_vec()));
                if self.fail_at == Some(writes.len()) {
                    return Err(StageError::Store);
                }
                Ok(())
            })
        }
    }

    fn key(len: usize) -> PastedAnthropicKey {
        PastedAnthropicKey::new(Zeroizing::new(format!(
            "sk-ant-api{}",
            "x".repeat(len - 10)
        )))
        .expect("valid fixture")
    }

    fn permit(len: u16) -> StagePermit {
        StagePermit::new(KeyStagePlan {
            id: KeyStageId([0xab; 16]),
            len,
        })
    }

    #[test]
    fn writes_exact_authority_names_and_2000_plus_48_bytes() {
        let store = FakeWriter::default();
        assert_eq!(
            store.write(permit(2048), key(2048)),
            Ok(KeyStageId([0xab; 16]))
        );
        let writes = store.writes.borrow();
        assert_eq!(writes.len(), 2);
        assert_eq!(
            writes[0].0,
            format!("{CREDENTIAL_SERVICE}/gen/anthropic/abababababababababababababababab/0")
        );
        assert_eq!(
            writes[1].0,
            format!("{CREDENTIAL_SERVICE}/gen/anthropic/abababababababababababababababab/1")
        );
        assert_eq!(
            writes[0].1,
            format!("sk-ant-api{}", "x".repeat(1990)).as_bytes()
        );
        assert_eq!(
            writes[1].1,
            b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
        );
    }

    #[test]
    fn trimmed_length_and_bytes_are_used() {
        let writer = FakeWriter::default();
        let key = PastedAnthropicKey::new(Zeroizing::new("\u{2003}sk-ant-api1234567\r\n".into()))
            .expect("valid fixture");
        assert_eq!(key.len(), 17);
        assert_eq!(writer.write(permit(17), key), Ok(KeyStageId([0xab; 16])));
        assert_eq!(writer.writes.borrow()[0].1, b"sk-ant-api1234567");
    }

    #[test]
    fn invalid_plans_and_mismatch_never_touch_store() {
        let writer = FakeWriter::default();
        for len in [0, 15, 2049, u16::MAX] {
            assert_eq!(
                writer.write(permit(len), key(16)),
                Err(StageError::InvalidPlan)
            );
        }
        assert_eq!(
            writer.write(permit(16), key(17)),
            Err(StageError::LengthMismatch)
        );
        assert_eq!(
            writer.write(permit(17), key(16)),
            Err(StageError::LengthMismatch)
        );
        assert!(writer.writes.borrow().is_empty());
    }

    #[test]
    fn store_failure_stops_without_rewriting() {
        for fail_at in [1, 2] {
            let writer = FakeWriter {
                fail_at: Some(fail_at),
                ..FakeWriter::default()
            };
            assert_eq!(
                writer.write(permit(2048), key(2048)),
                Err(StageError::Store)
            );
            assert_eq!(writer.writes.borrow().len(), fail_at);
        }
    }

    #[test]
    fn invalid_pastes_are_rejected_and_debug_redacts_content() {
        for text in [
            String::new(),
            "sk-ant-api12345".into(),
            format!("sk-ant-api{}", "x".repeat(2039)),
            "sk-ant-api123456 x".into(),
        ] {
            assert_eq!(
                PastedAnthropicKey::new(Zeroizing::new(text)).unwrap_err(),
                KeyRejection::Format
            );
        }
        let sentinel = "sk-ant-api-SENTINEL";
        let carrier =
            PastedAnthropicKey::new(Zeroizing::new(sentinel.into())).expect("valid fixture");
        let debug = format!("{carrier:?}");
        assert_eq!(debug, "PastedAnthropicKey(***)");
        assert!(!debug.contains(sentinel));
        assert!(!debug.contains(&format!("{:?}", sentinel.as_bytes())));
    }
}
