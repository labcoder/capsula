use super::*;
use crate::platform::credential::write_stage;
use kumochi_protocol::frame::{decode, encode_payload};
use kumochi_protocol::message::{ControlToBrain, KeyStagePlan};

const SESSION: u64 = 8;
const PLAN: KeyStageId = KeyStageId([17; 16]);
const KEY: &str = "sk-ant-api-fixture-only-sentinel";

fn model(view: KeyView) -> KeyPaste {
    let mut state = KeyPaste::default();
    state.receive(view);
    state
}

fn begin(state: &mut KeyPaste, key: &str) -> Attempt {
    state
        .begin(Some(Zeroizing::new(key.into())), SESSION, |_| true)
        .expect("begin")
}

fn plan(request: u64, len: u16) -> ControlToShell {
    ControlToShell::AnthropicKeyStage {
        request,
        result: Ok(KeyStagePlan { id: PLAN, len }),
    }
}

fn done(plan: KeyStageId, result: Result<(), KeyStageFailure>) -> ControlToShell {
    ControlToShell::AnthropicKeyStageDone { plan, result }
}

fn fake_success(state: &mut KeyPaste, at: Attempt) -> KeyWriteResult {
    state
        .planned(
            at.session,
            &plan(at.request, u16::try_from(KEY.len()).expect("bounded")),
        )
        .expect("write job")
        .run_with(|permit, key| write_stage(permit, key, |_, _| Ok(())))
}

#[test]
fn only_explicit_accepted_paste_sends_length_and_only_a_written_plan_is_sent() {
    let mut state = model(KeyView::Missing);
    let mut sent = Vec::new();
    let at = state
        .begin(Some(Zeroizing::new(format!(" {KEY}\r\n"))), SESSION, |op| {
            sent.push(op);
            true
        })
        .expect("accepted");
    let debug = format!("{state:?}");
    assert!(!debug.contains(KEY));
    let bytes = format!("{:?}", KEY.as_bytes());
    assert!(!debug.contains(bytes.trim_matches(['[', ']'])));

    let mut written = Vec::new();
    let result = state
        .planned(
            SESSION,
            &plan(at.request, u16::try_from(KEY.len()).expect("bounded")),
        )
        .expect("write job")
        .run_with(|permit, key| {
            write_stage(permit, key, |_, bytes| {
                written.extend_from_slice(bytes);
                Ok(())
            })
        });
    assert_eq!(written, KEY.as_bytes());
    state.written(&result, |op| {
        sent.push(op);
        true
    });
    let expected = [
        ConnectionOp::BeginAnthropicKeyStage {
            request: 1,
            len: u16::try_from(KEY.len()).expect("bounded"),
        },
        ConnectionOp::AnthropicKeyStaged { plan: PLAN },
    ];
    assert_eq!(sent, expected);
    for op in sent {
        let bytes = encode_payload(&ControlToBrain::Connection(op)).expect("encodes");
        assert_eq!(
            decode::<ControlToBrain>(&bytes).expect("decodes"),
            ControlToBrain::Connection(op)
        );
        assert!(
            !bytes
                .windows(KEY.len())
                .any(|window| window == KEY.as_bytes())
        );
    }
    state.done(SESSION, &done(PLAN, Ok(())));
    assert_eq!(state.view(), Some(KeyView::Checking));
    assert!(state.saved());
}

#[test]
fn no_explicit_valid_text_means_no_begin_or_fallback() {
    let mut state = model(KeyView::Missing);
    let cases = [
        (None, PasteFailure::Clipboard),
        (Some(String::new()), PasteFailure::Clipboard),
        (
            Some("x".into()),
            PasteFailure::Rejected(KeyRejection::Format),
        ),
        (
            Some("sk-ant-oat-synthetic".into()),
            PasteFailure::Rejected(KeyRejection::SubscriptionToken),
        ),
        (
            Some("sk-ant-admin-synthetic".into()),
            PasteFailure::Rejected(KeyRejection::AdminKey),
        ),
        (
            Some(format!("{}{}", KEY, " ".repeat(2048 - KEY.len() + 1))),
            PasteFailure::TooLong,
        ),
    ];
    for (text, failure) in cases {
        assert_eq!(
            state.begin(text.map(Zeroizing::new), SESSION, |_| panic!(
                "refused input reached the link"
            )),
            None
        );
        assert_eq!(state.failure(), Some(failure));
        assert!(!state.saving());
    }
    assert_eq!(state.view(), Some(KeyView::Missing));
    let edge = format!("{KEY}{}", "x".repeat(2048 - KEY.len()));
    assert_eq!(
        state.begin(Some(Zeroizing::new(edge)), SESSION, |op| {
            assert_eq!(
                op,
                ConnectionOp::BeginAnthropicKeyStage {
                    request: 1,
                    len: 2048
                }
            );
            true
        }),
        Some(Attempt {
            session: SESSION,
            request: 1
        })
    );
}

#[test]
fn stale_staging_replies_neither_consume_nor_revive_an_attempt() {
    let mut state = model(KeyView::Missing);
    let at = begin(&mut state, KEY);
    let len = u16::try_from(KEY.len()).expect("bounded");
    assert!(state.planned(SESSION + 1, &plan(at.request, len)).is_none());
    assert!(state.planned(SESSION, &plan(at.request + 1, len)).is_none());
    assert!(
        state
            .planned(
                SESSION + 1,
                &ControlToShell::AnthropicKeyStage {
                    request: at.request,
                    result: Err(KeyStageFailure::Unavailable),
                }
            )
            .is_none()
    );
    let old_write = fake_success(&mut state, at);
    assert!(state.planned(SESSION, &plan(at.request, len)).is_none());
    state.disconnected();
    state.receive(KeyView::Missing);
    let new = begin(&mut state, KEY);
    state.written(&old_write, |_| panic!("stale write sent Staged"));
    assert!(!state.expire(at));
    let current = fake_success(&mut state, new);
    state.written(&current, |op| {
        assert_eq!(op, ConnectionOp::AnthropicKeyStaged { plan: PLAN });
        true
    });
    state.done(SESSION + 1, &done(PLAN, Err(KeyStageFailure::Unavailable)));
    state.done(SESSION, &done(KeyStageId([0; 16]), Ok(())));
    assert!(state.saving());
    assert!(state.expire(new));
    assert_eq!(state.failure(), Some(PasteFailure::Unconfirmed));
    state.done(SESSION, &done(PLAN, Ok(())));
    assert_eq!(state.view(), Some(KeyView::Missing));
    assert!(!state.saved());
    assert!(!state.can_paste());
    assert!(state.check_available());
}

#[test]
fn a_partial_write_and_failed_enqueues_end_locally() {
    let mut state = model(KeyView::Ready);
    assert_eq!(
        state.begin(Some(Zeroizing::new(KEY.into())), SESSION, |_| false),
        None
    );
    assert_eq!(state.failure(), Some(PasteFailure::Offline));
    assert!(state.planned(SESSION, &plan(1, 32)).is_none());

    let long = format!("{KEY}{}", "x".repeat(2048 - KEY.len()));
    let at = begin(&mut state, &long);
    let mut chunks = Vec::new();
    let partial = state
        .planned(SESSION, &plan(at.request, 2048))
        .expect("job")
        .run_with(|permit, key| {
            write_stage(permit, key, |_, bytes| {
                chunks.push(bytes.len());
                if chunks.len() == 2 {
                    Err(StageError::Store)
                } else {
                    Ok(())
                }
            })
        });
    assert_eq!(chunks, [2000, 48]);
    assert_eq!(partial.result, Err(StageError::Store));
    state.written(&partial, |_| panic!("partial write sent Staged"));
    assert_eq!(state.failure(), Some(PasteFailure::Unsaved));
    assert_eq!(state.view(), Some(KeyView::Ready));

    let at = begin(&mut state, KEY);
    let result = fake_success(&mut state, at);
    state.written(&result, |_| false);
    state.done(SESSION, &done(PLAN, Ok(())));
    assert_eq!(state.failure(), Some(PasteFailure::Offline));
    assert_eq!(state.view(), Some(KeyView::Ready));
    assert!(!state.saving());
}

#[test]
fn a_writer_returning_a_different_identity_does_not_admit_adoption() {
    let mut state = model(KeyView::Ready);
    let at = begin(&mut state, KEY);
    let wrong = state
        .planned(SESSION, &plan(at.request, 32))
        .expect("job")
        .run_with(|_, _| Ok(KeyStageId([1; 16])));
    assert_eq!(wrong.result, Err(StageError::IdentityMismatch));
    state.written(&wrong, |_| panic!("the wrong identity sent Staged"));
    assert_eq!(state.failure(), Some(PasteFailure::Unsaved));
    assert_eq!(state.view(), Some(KeyView::Ready));
}

#[test]
fn failed_adoption_preserves_a_key_but_catalog_rejection_is_removable() {
    let mut state = model(KeyView::Ready);
    let old_revision = state.remove_revision().expect("old removal confirmation");
    let at = begin(&mut state, KEY);
    let result = fake_success(&mut state, at);
    state.written(&result, |_| true);
    state.done(SESSION, &done(PLAN, Err(KeyStageFailure::Changed)));
    assert_eq!(state.view(), Some(KeyView::Ready));
    assert_eq!(state.failure(), Some(PasteFailure::Unsaved));
    state.remove(old_revision, |_| panic!("stale confirmation"));

    let at = begin(&mut state, KEY);
    let result = fake_success(&mut state, at);
    state.written(&result, |_| true);
    state.done(SESSION, &done(PLAN, Ok(())));
    assert_eq!(state.view(), Some(KeyView::Checking));
    state.receive(KeyView::Rejected(KeyRejection::Refused));
    assert!(state.saved());
    let revision = state
        .remove_revision()
        .expect("saved rejected key can be removed");
    state.remove(revision, |op| {
        assert_eq!(op, ConnectionOp::RemoveAnthropicKey);
        true
    });
    assert_eq!(state.removal(), Removal::Waiting);
    state.receive(KeyView::Unconfirmed);
    assert_eq!(state.removal(), Removal::Retry);
    state.check(|op| {
        assert_eq!(op, ConnectionOp::CheckAnthropicKey);
        true
    });
    assert_eq!(state.removal(), Removal::Waiting);
    state.receive(KeyView::Missing);
    assert_eq!(state.removal(), Removal::None);
    assert!(!state.saved());
}

#[test]
fn expiry_and_disconnect_drop_the_waiting_plan_admission() {
    let mut state = model(KeyView::Missing);
    let old = begin(&mut state, KEY);
    assert!(state.expire(old));
    assert_eq!(state.failure(), Some(PasteFailure::Unsaved));
    assert!(state.planned(SESSION, &plan(old.request, 32)).is_none());
    let waiting = begin(&mut state, KEY);
    state.disconnected();
    assert_eq!(state.view(), None);
    assert!(!state.saving());
    assert!(state.planned(SESSION, &plan(waiting.request, 32)).is_none());
}
