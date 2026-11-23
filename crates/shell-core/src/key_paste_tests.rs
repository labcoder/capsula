use super::*;
use crate::platform::credential::write_stage;
use kumochi_protocol::frame::{decode, encode_payload};
use kumochi_protocol::message::{ControlToBrain, KeyStagePlan};

const SESSION: u64 = 8;
const PLAN: KeyStageId = KeyStageId([17; 16]);
const KEY: &str = "sk-ant-api-fixture-only-sentinel";

fn model(view: KeyView) -> KeyPaste {
    let mut state = KeyPaste::default();
    state.receive(view, (view != KeyView::Missing).then_some(1));
    state
}

fn begin(state: &mut KeyPaste, key: &str) -> Attempt {
    state
        .begin(
            Some(Zeroizing::new(key.into())),
            SESSION,
            Instant::now(),
            |_| true,
        )
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
    let now = at
        .deadline
        .checked_sub(PASTE_TIMEOUT)
        .expect("attempt start");
    state
        .planned(
            at.session,
            &plan(at.request, u16::try_from(KEY.len()).expect("bounded")),
            now,
        )
        .expect("write job")
        .run_with(now, |permit, key| write_stage(permit, key, |_, _| Ok(())))
}

#[test]
fn only_explicit_accepted_paste_sends_length_and_only_a_written_plan_is_sent() {
    let mut state = model(KeyView::Missing);
    let mut sent = Vec::new();
    let at = state
        .begin(
            Some(Zeroizing::new(format!(" {KEY}\r\n"))),
            SESSION,
            Instant::now(),
            |op| {
                sent.push(op);
                true
            },
        )
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
            Instant::now(),
        )
        .expect("write job")
        .run_with(Instant::now(), |permit, key| {
            write_stage(permit, key, |_, bytes| {
                written.extend_from_slice(bytes);
                Ok(())
            })
        });
    assert_eq!(written, KEY.as_bytes());
    state.written(&result, Instant::now(), |op| {
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
    state.done(SESSION, &done(PLAN, Ok(())), Instant::now());
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
            state.begin(
                text.map(Zeroizing::new),
                SESSION,
                Instant::now(),
                |_| panic!("refused input reached the link")
            ),
            None
        );
        assert_eq!(state.failure(), Some(failure));
        assert!(!state.saving());
    }
    assert_eq!(state.view(), Some(KeyView::Missing));
    let edge = format!("{KEY}{}", "x".repeat(2048 - KEY.len()));
    let now = Instant::now();
    assert_eq!(
        state.begin(Some(Zeroizing::new(edge)), SESSION, now, |op| {
            assert_eq!(
                op,
                ConnectionOp::BeginAnthropicKeyStage {
                    request: 7,
                    len: 2048
                }
            );
            true
        }),
        Some(Attempt {
            session: SESSION,
            request: 7,
            deadline: now + Duration::from_secs(30),
        })
    );
}

#[test]
fn clipboard_and_staging_use_one_deadline_and_one_click() {
    let mut state = model(KeyView::Missing);
    let now = Instant::now();
    let read = state.start_read(SESSION, now).expect("first click");
    let at = read.attempt();
    assert_eq!(at.deadline, now + Duration::from_secs(30));
    assert!(state.saving() && read.is_current());
    assert!(state.start_read(SESSION, now).is_none());
    let text = || Ok(Zeroizing::new(KEY.into()));
    let len = u16::try_from(KEY.len()).expect("bounded");
    let before = at
        .deadline
        .checked_sub(Duration::from_millis(1))
        .expect("before deadline");
    assert!(state.clipboard_read(at, text(), before, |op| {
        assert_eq!(op, ConnectionOp::BeginAnthropicKeyStage { request: 1, len });
        true
    }));
    assert!(!read.is_current());
    assert!(!state.clipboard_read(at, text(), before, |_| panic!("duplicate read sent Begin")));
    assert!(state.planned(SESSION, &plan(1, len), at.deadline).is_none());
    assert_eq!(state.failure(), Some(PasteFailure::Unsaved));
}

#[test]
fn retired_clipboard_reads_cannot_start_a_new_session_or_revive_a_timed_out_click() {
    let mut state = model(KeyView::Ready);
    let now = Instant::now();
    let old = state.start_read(SESSION, now).expect("first click");
    state.disconnected();
    assert!(!old.is_current());
    state.receive(KeyView::Ready, Some(1));
    let pending = state.start_read(SESSION + 1, now).expect("new click");
    let at = pending.attempt();
    let text = || Ok(Zeroizing::new(KEY.into()));
    assert!(
        !state.clipboard_read(old.attempt(), text(), now, |_| panic!(
            "old link sent Begin"
        ))
    );
    assert!(pending.is_current());
    assert!(!state.clipboard_read(at, text(), at.deadline, |_| panic!("late read sent Begin")));
    assert_eq!(state.failure(), Some(PasteFailure::Clipboard));
    assert!(!pending.is_current());
    let retry = state.start_read(SESSION + 1, at.deadline).expect("retry");
    assert!(!state.clipboard_read(
        retry.attempt(),
        Err(ClipboardRefusal::Changed),
        at.deadline,
        |_| panic!("changed text sent Begin")
    ));
    assert_eq!(state.failure(), Some(PasteFailure::Clipboard));
    assert!(!state.saving());
}

#[test]
fn stale_staging_replies_neither_consume_nor_revive_an_attempt() {
    let mut state = model(KeyView::Missing);
    let at = begin(&mut state, KEY);
    let len = u16::try_from(KEY.len()).expect("bounded");
    assert!(
        state
            .planned(SESSION + 1, &plan(at.request, len), Instant::now())
            .is_none()
    );
    assert!(
        state
            .planned(SESSION, &plan(at.request + 1, len), Instant::now())
            .is_none()
    );
    assert!(
        state
            .planned(
                SESSION + 1,
                &ControlToShell::AnthropicKeyStage {
                    request: at.request,
                    result: Err(KeyStageFailure::Unavailable),
                },
                Instant::now(),
            )
            .is_none()
    );
    let old_write = fake_success(&mut state, at);
    assert!(
        state
            .planned(SESSION, &plan(at.request, len), Instant::now())
            .is_none()
    );
    state.disconnected();
    state.receive(KeyView::Missing, None);
    let new = begin(&mut state, KEY);
    state.written(&old_write, Instant::now(), |_| {
        panic!("stale write sent Staged")
    });
    assert!(!state.expire(at, at.deadline));
    let current = fake_success(&mut state, new);
    state.written(&current, Instant::now(), |op| {
        assert_eq!(op, ConnectionOp::AnthropicKeyStaged { plan: PLAN });
        true
    });
    state.done(
        SESSION + 1,
        &done(PLAN, Err(KeyStageFailure::Unavailable)),
        Instant::now(),
    );
    state.done(SESSION, &done(KeyStageId([0; 16]), Ok(())), Instant::now());
    assert!(state.saving());
    assert!(state.expire(new, new.deadline));
    assert_eq!(state.failure(), Some(PasteFailure::Unconfirmed));
    state.done(SESSION, &done(PLAN, Ok(())), Instant::now());
    assert_eq!(state.view(), Some(KeyView::Missing));
    assert!(!state.saved());
    assert!(!state.can_paste());
    assert!(state.check_available());

    let mut state = model(KeyView::Missing);
    let old = begin(&mut state, KEY);
    assert!(state.expire(old, old.deadline));
    assert_eq!(state.failure(), Some(PasteFailure::Unsaved));
    assert!(
        state
            .planned(SESSION, &plan(old.request, len), Instant::now())
            .is_none()
    );
    let waiting = begin(&mut state, KEY);
    state.disconnected();
    assert_eq!(state.view(), None);
    assert!(!state.saving());
    assert!(
        state
            .planned(SESSION, &plan(waiting.request, len), Instant::now())
            .is_none()
    );
}

#[test]
fn a_partial_write_and_failed_enqueues_end_locally() {
    let mut state = model(KeyView::Ready);
    assert_eq!(
        state.begin(
            Some(Zeroizing::new(KEY.into())),
            SESSION,
            Instant::now(),
            |_| false
        ),
        None
    );
    assert_eq!(state.failure(), Some(PasteFailure::Offline));
    assert!(
        state
            .planned(SESSION, &plan(1, 32), Instant::now())
            .is_none()
    );

    let long = format!("{KEY}{}", "x".repeat(2048 - KEY.len()));
    let at = begin(&mut state, &long);
    let mut chunks = Vec::new();
    let partial = state
        .planned(SESSION, &plan(at.request, 2048), Instant::now())
        .expect("job")
        .run_with(Instant::now(), |permit, key| {
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
    state.written(&partial, Instant::now(), |_| {
        panic!("partial write sent Staged")
    });
    assert_eq!(state.failure(), Some(PasteFailure::Unsaved));
    assert_eq!(state.view(), Some(KeyView::Ready));

    let at = begin(&mut state, KEY);
    let result = fake_success(&mut state, at);
    state.written(&result, Instant::now(), |_| false);
    state.done(SESSION, &done(PLAN, Ok(())), Instant::now());
    assert_eq!(state.failure(), Some(PasteFailure::Offline));
    assert_eq!(state.view(), Some(KeyView::Ready));
    assert!(!state.saving());
}

#[test]
fn a_writer_returning_a_different_identity_does_not_admit_adoption() {
    let mut state = model(KeyView::Ready);
    let at = begin(&mut state, KEY);
    let wrong = state
        .planned(SESSION, &plan(at.request, 32), Instant::now())
        .expect("job")
        .run_with(Instant::now(), |_, _| Ok(KeyStageId([1; 16])));
    assert_eq!(wrong.result, Err(StageError::IdentityMismatch));
    state.written(&wrong, Instant::now(), |_| {
        panic!("the wrong identity sent Staged")
    });
    assert_eq!(state.failure(), Some(PasteFailure::Unsaved));
    assert_eq!(state.view(), Some(KeyView::Ready));
}

#[test]
fn the_deadline_holds_before_the_timer_callback_in_each_phase() {
    let now = Instant::now();
    let mut state = model(KeyView::Missing);
    let at = state
        .begin(Some(Zeroizing::new(KEY.into())), SESSION, now, |_| true)
        .expect("attempt");
    let deadline = now + Duration::from_secs(30);
    let just_before = |at: Attempt| {
        at.deadline
            .checked_sub(Duration::from_nanos(1))
            .expect("one tick before the deadline")
    };
    let len = u16::try_from(KEY.len()).expect("bounded");
    assert_eq!(at.deadline, deadline);
    assert!(!state.expire(at, just_before(at)));
    assert!(
        state
            .planned(SESSION, &plan(at.request, len), deadline)
            .is_none()
    );
    assert_eq!(state.failure(), Some(PasteFailure::Unsaved));

    let at = begin(&mut state, KEY);
    let result = state
        .planned(SESSION, &plan(at.request, len), just_before(at))
        .expect("plan before the deadline")
        .run_with(at.deadline, |_, _| {
            panic!("a queued writer started at the deadline")
        });
    assert_eq!(result.result, Err(StageError::Expired));
    state.written(&result, at.deadline, |_| {
        panic!("expired write sent Staged")
    });
    assert_eq!(state.failure(), Some(PasteFailure::Unsaved));

    let at = begin(&mut state, KEY);
    let result = fake_success(&mut state, at);
    state.written(&result, at.deadline, |_| panic!("late write sent Staged"));
    assert_eq!(state.failure(), Some(PasteFailure::Unsaved));

    let at = begin(&mut state, KEY);
    let result = fake_success(&mut state, at);
    state.written(&result, just_before(at), |_| true);
    state.done(SESSION, &done(PLAN, Ok(())), at.deadline);
    assert_eq!(state.failure(), Some(PasteFailure::Unconfirmed));
    assert!(!state.saving());
    assert!(!state.can_paste());
    assert!(state.check_available());
}

#[test]
fn a_ready_to_ready_replacement_invalidates_the_open_confirmation() {
    let mut state = model(KeyView::Ready);
    let old = state.removal_confirmation().expect("old confirmation");
    assert!(state.receive(KeyView::Ready, Some(2)));
    state.remove(old, |_| {
        panic!("confirmation was for the previous authority revision")
    });
    let current = state.removal_confirmation().expect("new confirmation");
    state.remove(current, |op| {
        assert_eq!(
            op,
            ConnectionOp::RemoveAnthropicKey {
                revision: 2,
                request: 1
            }
        );
        true
    });
    assert_eq!(state.removal(), Removal::Waiting);
}

#[test]
fn failed_adoption_preserves_a_key_but_catalog_rejection_is_removable() {
    let mut state = model(KeyView::Ready);
    let old_revision = state
        .removal_confirmation()
        .expect("old removal confirmation");
    let at = begin(&mut state, KEY);
    let result = fake_success(&mut state, at);
    state.written(&result, Instant::now(), |_| true);
    state.done(
        SESSION,
        &done(PLAN, Err(KeyStageFailure::Changed)),
        Instant::now(),
    );
    assert_eq!(state.view(), Some(KeyView::Ready));
    assert_eq!(state.failure(), Some(PasteFailure::Unsaved));
    state.remove(old_revision, |_| panic!("stale confirmation"));

    let at = begin(&mut state, KEY);
    let result = fake_success(&mut state, at);
    state.written(&result, Instant::now(), |_| true);
    state.done(SESSION, &done(PLAN, Ok(())), Instant::now());
    assert_eq!(state.view(), Some(KeyView::Checking));
    state.receive(KeyView::Rejected(KeyRejection::Refused), Some(1));
    assert!(state.saved());
    let revision = state
        .removal_confirmation()
        .expect("saved rejected key can be removed");
    state.remove(revision, |op| {
        assert_eq!(
            op,
            ConnectionOp::RemoveAnthropicKey {
                revision: 1,
                request: 3
            }
        );
        true
    });
    assert_eq!(state.removal(), Removal::Waiting);
    state.receive(KeyView::Unconfirmed, Some(2));
    state.removed(&ControlToShell::AnthropicKeyRemoved {
        request: 2,
        result: Err(KeyRemovalFailure::Unconfirmed),
    });
    assert_eq!(state.removal(), Removal::Waiting);
    state.removed(&ControlToShell::AnthropicKeyRemoved {
        request: 3,
        result: Err(KeyRemovalFailure::Unconfirmed),
    });
    assert_eq!(state.removal(), Removal::Retry);
    state.check(|op| {
        assert_eq!(
            op,
            ConnectionOp::RemoveAnthropicKey {
                revision: 1,
                request: 4
            }
        );
        true
    });
    assert_eq!(state.removal(), Removal::Waiting);
    state.receive(KeyView::Missing, None);
    assert_eq!(state.removal(), Removal::Waiting);
    state.removed(&ControlToShell::AnthropicKeyRemoved {
        request: 4,
        result: Ok(()),
    });
    assert_eq!(state.removal(), Removal::None);
    assert!(!state.saved());
}

#[test]
fn disconnect_retires_the_check_without_releasing_an_admitted_save() {
    let mut state = model(KeyView::Ready);
    let at = begin(&mut state, KEY);
    let written = fake_success(&mut state, at);
    state.written(&written, Instant::now(), |_| true);
    state.disconnected();
    assert_eq!(state.failure(), Some(PasteFailure::Unconfirmed));
    assert!(!state.check_available());
    state.receive(KeyView::Missing, None);
    state.check(|op| {
        assert_eq!(op, ConnectionOp::ReconcileAnthropicKey { request: 2 });
        true
    });
    assert!(!state.check_available());
    state.disconnected();
    state.receive(KeyView::Ready, Some(2));
    state.check(|op| {
        assert_eq!(op, ConnectionOp::ReconcileAnthropicKey { request: 3 });
        true
    });
    state.reconciled(&ControlToShell::AnthropicKeyReconciled {
        request: 2,
        read: Some(KeyRecoveryRead::Missing),
    });
    assert_eq!(state.failure(), Some(PasteFailure::Unconfirmed));
    assert!(!state.saved());
    assert!(!state.can_paste());
    assert_eq!(state.removal_confirmation(), None);
    state.reconciled(&ControlToShell::AnthropicKeyReconciled {
        request: 3,
        read: None,
    });
    assert!(state.check_available());
    assert!(!state.saved());
    assert!(!state.can_paste());
}

#[test]
fn a_local_recovery_read_only_unlocks_its_revision_during_catalog_checking() {
    let mut state = model(KeyView::Ready);
    let at = begin(&mut state, KEY);
    let written = fake_success(&mut state, at);
    state.written(&written, Instant::now(), |_| true);
    assert!(state.expire(at, at.deadline));
    state.check(|_| true);
    state.receive(KeyView::Checking, Some(2));
    assert!(!state.can_paste());
    state.reconciled(&ControlToShell::AnthropicKeyReconciled {
        request: 2,
        read: Some(KeyRecoveryRead::Present {
            revision: 2,
            rejection: None,
        }),
    });
    assert_eq!(state.view(), Some(KeyView::Checking));
    assert!(!state.saved());
    assert!(state.can_paste());
    assert!(state.removal_confirmation().is_some());
    state.reconciled(&ControlToShell::AnthropicKeyReconciled {
        request: 2,
        read: Some(KeyRecoveryRead::Missing),
    });
    assert_eq!(state.view(), Some(KeyView::Checking));
    state.receive(KeyView::Checking, Some(2));
    assert!(state.can_paste());
    state.receive(KeyView::Checking, Some(3));
    assert!(!state.can_paste());
    assert_eq!(state.removal_confirmation(), None);
}
