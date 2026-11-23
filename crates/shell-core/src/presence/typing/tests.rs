use kumochi_protocol::message::{DataToBrain, Incarnation, ThreadId};

use super::{Admission, Hide, Notice, Submission, Typing, admit, notice, restore, submit};
use crate::link::{LinkHandle, Session};
use crate::runs::{Refused, Runs};
use crate::strings::{Strings, Text};

#[test]
fn reopening_active_input_keeps_blur_dismissal() {
    let mut typing = Typing::default();
    assert!(typing.open(), "a hidden panel just opened");
    typing.activated();
    assert!(!typing.open(), "an already-open panel stays open");
    assert!(typing.close(Hide::Deactivated), "reopening kept activation");
    assert!(!typing.visible, "blur hid the panel");
    assert!(typing.open(), "the panel reopens after blur");
    assert!(
        !typing.close(Hide::Deactivated),
        "blur before activation is ignored"
    );
    assert!(typing.visible, "the panel is still available to click");
    assert!(
        typing.close(Hide::Dismiss),
        "cancel hides an unactivated panel"
    );
    typing.activated();
    assert!(
        !typing.close(Hide::Deactivated),
        "a late blur doesn't hide it twice"
    );
}

#[test]
fn explicit_cancel_can_restore_foreground_but_blur_cannot() {
    assert!(Hide::Dismiss.restores_foreground(), "cancel returns focus");
    assert!(
        !Hide::Deactivated.restores_foreground(),
        "blur leaves focus with the new owner"
    );
}

#[test]
fn blank_enter_preserves_the_retained_notice_until_a_turn_is_sent() {
    let (link, sent) = LinkHandle::recording();
    let session = Session::detached(1, Incarnation(1));
    let mut runs = Runs::default();
    runs.connected(1);
    runs.settings(ThreadId(3));
    assert_eq!(admit(&link, Some(&session)), Admission::Proceed);
    let mut error = Some("Couldn't save that setting. Try again.".to_owned());
    let mut refusal = None;

    let blank = submit(&mut runs, &link, Some(&session), "  ");
    assert_eq!(blank, Submission::Blank);
    assert_eq!(sent.try_recv(), Err(async_channel::TryRecvError::Empty));
    blank.update_notice(&mut error, &mut refusal);
    assert!(matches!(
        notice(refusal, error.as_deref()),
        Notice::Recovery("Couldn't save that setting. Try again.")
    ));
    assert_eq!(refusal, None);

    Submission::Refused(Text::TurnBusy).update_notice(&mut error, &mut refusal);
    assert!(matches!(
        notice(refusal, error.as_deref()),
        Notice::Refusal(Text::TurnBusy)
    ));
    let submitted = submit(&mut runs, &link, Some(&session), "a new draft");
    assert_eq!(submitted, Submission::Sent);
    let DataToBrain::TextSubmit { text, .. } = sent.try_recv().expect("the typed turn") else {
        panic!("expected a typed turn");
    };
    assert_eq!(text.as_str(), "a new draft");
    submitted.update_notice(&mut error, &mut refusal);
    assert_eq!((error, refusal), (None, None));
    assert!(matches!(notice(refusal, None::<&str>), Notice::Fallback));
}

#[test]
fn a_local_refusal_preserves_the_dock_facing_brain_error() {
    let strings = Strings::english();
    let saved = "Your settings file can't be read, so changes aren't saved. Repair or delete config.toml, then restart.";
    let mut error = Some(saved.to_owned());
    let mut refusal = None;
    let link = LinkHandle::detached();
    let session = Session::detached(1, Incarnation(1));
    let mut runs = Runs::default();
    runs.connected(1);
    runs.settings(ThreadId(3));

    let refused = submit(&mut runs, &link, Some(&session), "keep this draft");
    assert_eq!(refused, Submission::Refused(Text::TurnOffline));
    for (result, expected) in [
        (refused, "Not sent: the background process isn't connected."),
        (
            Submission::from(Err(Refused::TooLong)),
            "That message is too long to send.",
        ),
    ] {
        result.update_notice(&mut error, &mut refusal);
        assert_eq!(error.as_deref(), Some(saved));
        let Notice::Refusal(text) = notice(refusal, error.as_deref()) else {
            panic!("the local refusal should win over recovery");
        };
        assert_eq!(strings.get(text), expected);
    }
    assert!(matches!(
        notice(None, error.as_deref()),
        Notice::Recovery(text) if text == saved
    ));
}

#[test]
fn a_pre_settings_refusal_never_becomes_a_dock_or_tasks_error() {
    let strings = Strings::english();
    let mut runs = Runs::default();
    runs.connected(1);
    let (link, sent) = LinkHandle::recording();
    let session = Session::detached(1, Incarnation(1));
    let result = submit(&mut runs, &link, Some(&session), "typed");
    assert_eq!(result, Submission::Refused(Text::TurnOffline));
    assert_eq!(sent.try_recv(), Err(async_channel::TryRecvError::Empty));
    let mut error: Option<String> = None;
    let mut refusal = None;
    result.update_notice(&mut error, &mut refusal);
    assert_eq!(error, None);
    assert_eq!(refusal, Some(Text::TurnOffline));
    let Notice::Refusal(text) = notice(refusal, error.as_deref()) else {
        panic!("the local refusal should be visible");
    };
    assert_eq!(
        strings.get(text),
        "Not sent: the background process isn't connected."
    );
}

#[test]
fn recovery_fills_an_empty_draft_without_overwriting_new_input() {
    let restored = restore("", "Exact draft: café ✓".into()).expect("an empty draft");
    assert_eq!(restored.as_str(), "Exact draft: café ✓");
    for draft in ["newer draft", " "] {
        assert_eq!(restore(draft, "older failed turn".into()), None);
    }
}
