//! Settings drafts and ordered save answers, independent of the UI and transport.

use std::collections::VecDeque;
use std::fmt;

use kumochi_protocol::message::{
    AssistantName, NameError, SettingsOp, SettingsView, UserError, UserName,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Field {
    Assistant,
    User,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Update {
    Answer,
    Unsolicited,
}

#[derive(Clone, Copy, Debug)]
enum Draft {
    Reset,
    Preserve,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Error {
    #[error("{0}")]
    Validation(NameError),
    #[error("the brain refused the settings change")]
    Brain(UserError),
    #[error("the brain is offline")]
    Offline,
}

#[derive(Default)]
pub(crate) struct FieldState {
    pub(crate) saving: bool,
    pub(crate) error: Option<Error>,
    value: String,
    authoritative: String,
}

impl fmt::Debug for FieldState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FieldState")
            .field("saving", &self.saving)
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Completion {
    pub(crate) failed: bool,
    pub(crate) field: Field,
}

#[derive(Debug)]
struct Pending {
    field: Field,
    failed: bool,
    reset_draft: bool,
}

#[derive(Default)]
pub(crate) struct Settings {
    view: Option<SettingsView>,
    notice: Option<UserError>,
    notice_field: Option<Field>,
    assistant: FieldState,
    user: FieldState,
    pending: VecDeque<Pending>,
}

impl fmt::Debug for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Settings")
            .field("online", &self.view.is_some())
            .field("assistant", &self.assistant)
            .field("user", &self.user)
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

impl Settings {
    pub(crate) fn receive(
        &mut self,
        current: Option<u64>,
        generation: u64,
        view: SettingsView,
        update: Update,
    ) -> bool {
        if current != Some(generation) {
            return false;
        }
        let _completion = self.set_view(view, update);
        true
    }

    pub(crate) fn set_view(&mut self, view: SettingsView, update: Update) -> Option<Completion> {
        let sync_assistant = !self.assistant.saving && !self.editing(Field::Assistant);
        let sync_user = !self.user.saving && !self.editing(Field::User);
        view.assistant_name
            .as_str()
            .clone_into(&mut self.assistant.authoritative);
        view.user_name
            .as_ref()
            .map_or("", UserName::as_str)
            .clone_into(&mut self.user.authoritative);
        if sync_assistant {
            self.assistant
                .value
                .clone_from(&self.assistant.authoritative);
        }
        if sync_user {
            self.user.value.clone_from(&self.user.authoritative);
        }
        self.view = Some(view);
        if update == Update::Unsolicited {
            return None;
        }
        let pending = self.pending.pop_front()?;
        let state = self.field_mut(pending.field);
        state.saving = false;
        if pending.reset_draft {
            state.value.clone_from(&state.authoritative);
        }
        if !pending.failed && (pending.reset_draft || matches!(state.error, Some(Error::Brain(_))))
        {
            state.error = None;
        }
        if !pending.failed && self.notice_field == Some(pending.field) {
            self.clear_notice();
        }
        Some(Completion {
            failed: pending.failed,
            field: pending.field,
        })
    }

    pub(crate) fn view(&self) -> Option<&SettingsView> {
        self.view.as_ref()
    }

    pub(crate) fn notice(&self) -> Option<&UserError> {
        self.notice.as_ref()
    }

    pub(crate) fn clear_notice(&mut self) {
        self.notice = None;
        self.notice_field = None;
    }

    pub(crate) fn field(&self, field: Field) -> &FieldState {
        match field {
            Field::Assistant => &self.assistant,
            Field::User => &self.user,
        }
    }

    fn field_mut(&mut self, field: Field) -> &mut FieldState {
        match field {
            Field::Assistant => &mut self.assistant,
            Field::User => &mut self.user,
        }
    }

    pub(crate) fn value(&self, field: Field) -> &str {
        &self.field(field).value
    }

    pub(crate) fn editing(&self, field: Field) -> bool {
        let state = self.field(field);
        state.value != state.authoritative
    }

    pub(crate) fn change(&mut self, field: Field, value: &str) {
        let state = self.field_mut(field);
        if state.value == value {
            return;
        }
        value.clone_into(&mut state.value);
        state.error = None;
        for pending in &mut self.pending {
            if pending.field == field {
                pending.reset_draft = false;
            }
        }
    }

    pub(crate) fn cancel(&mut self, field: Field) {
        let value = self.field(field).authoritative.clone();
        self.change(field, &value);
    }

    pub(crate) fn save(&mut self, field: Field, send: impl FnOnce(SettingsOp) -> bool) -> bool {
        if self.field(field).saving {
            return false;
        }
        let value = self.value(field);
        let op = match field {
            Field::Assistant => AssistantName::new(value).map(SettingsOp::SetAssistantName),
            Field::User if value.trim().is_empty() => Ok(SettingsOp::SetUserName(None)),
            Field::User => UserName::new(value).map(|name| SettingsOp::SetUserName(Some(name))),
        };
        match op {
            Ok(_) if value.trim() == self.field(field).authoritative => {
                let state = self.field_mut(field);
                state.value.clone_from(&state.authoritative);
                false
            }
            Ok(op) => self.send(field, op, Draft::Reset, send),
            Err(error) => {
                self.field_mut(field).error = Some(Error::Validation(error));
                false
            }
        }
    }

    /// Inline Kumo renames share the settings FIFO without replacing an unrelated draft.
    pub(crate) fn send_assistant(
        &mut self,
        name: AssistantName,
        send: impl FnOnce(SettingsOp) -> bool,
    ) -> bool {
        if name.as_str() == self.assistant.authoritative {
            return false;
        }
        self.send(
            Field::Assistant,
            SettingsOp::SetAssistantName(name),
            if self.editing(Field::Assistant) {
                Draft::Preserve
            } else {
                Draft::Reset
            },
            send,
        )
    }

    fn send(
        &mut self,
        field: Field,
        op: SettingsOp,
        draft: Draft,
        send: impl FnOnce(SettingsOp) -> bool,
    ) -> bool {
        if self.field(field).saving {
            return false;
        }
        if self.view.is_none() || !send(op) {
            self.field_mut(field).error = Some(Error::Offline);
            return false;
        }
        self.field_mut(field).saving = true;
        self.pending.push_back(Pending {
            field,
            failed: false,
            reset_draft: matches!(draft, Draft::Reset),
        });
        true
    }

    pub(crate) fn error(&mut self, error: UserError) -> Option<Field> {
        self.notice = Some(error.clone());
        self.notice_field = self.pending.front().map(|pending| pending.field);
        let pending = self.pending.front_mut()?;
        pending.failed = true;
        let field = pending.field;
        self.field_mut(field).error = Some(Error::Brain(error));
        Some(field)
    }

    pub(crate) fn disconnected(&mut self) {
        self.view = None;
        self.pending.clear();
        self.assistant.saving = false;
        self.user.saving = false;
    }
}

#[cfg(test)]
mod session_tests;

#[cfg(test)]
mod tests {
    use kumochi_protocol::message::{ErrorKey, ThreadId};

    use super::{
        AssistantName, Completion, Error, Field, NameError, Settings, SettingsOp, SettingsView,
        Update, UserName,
    };

    fn view(assistant: &str, user: Option<&str>) -> SettingsView {
        SettingsView {
            assistant_name: AssistantName::new(assistant).expect("valid assistant name"),
            user_name: user.map(|name| UserName::new(name).expect("valid user name")),
            onboarded: true,
            main_thread: ThreadId(1),
        }
    }

    fn connected() -> Settings {
        let mut settings = Settings::default();
        assert_eq!(
            settings.set_view(view("Kumo", Some("Oscar")), Update::Unsolicited),
            None
        );
        settings
    }

    #[test]
    fn unchanged_normalized_names_never_save_even_with_an_error() {
        let mut settings = connected();
        for field in [Field::Assistant, Field::User] {
            assert!(
                !settings.save(field, |_| panic!("an unchanged field was sent")),
                "focus and blur do not write settings"
            );
            assert!(!settings.field(field).saving, "no save is pending");
            assert_eq!(settings.field(field).error, None);
        }
        for (field, value) in [(Field::Assistant, " Kumo "), (Field::User, " Oscar ")] {
            settings.change(field, value);
            assert!(
                !settings.save(field, |_| panic!("a normalized unchanged name was sent")),
                "whitespace does not make a name change"
            );
            assert!(
                !settings.field(field).saving,
                "no normalized save is pending"
            );
            assert_eq!(settings.field(field).error, None);
            assert!(
                !settings.editing(field),
                "a no-op save settles the draft, so later views and renames reach it"
            );
        }
        assert!(
            !settings.send_assistant(AssistantName::new(" Kumo ").expect("valid"), |_| panic!(
                "an unchanged inline rename was sent"
            )),
            "an inline rename also skips the saved name"
        );
        assert_eq!(settings.error(ErrorKey::SettingsRefused.into()), None);
        settings.change(Field::User, "Alex");
        assert!(settings.save(Field::User, |_| true), "the edit is sent");
        assert_eq!(
            settings.error(ErrorKey::SettingsRefused.into()),
            Some(Field::User)
        );
        assert_eq!(
            settings.set_view(view("Kumo", Some("Oscar")), Update::Answer),
            Some(Completion {
                failed: true,
                field: Field::User
            })
        );
        assert_eq!(settings.value(Field::User), "Oscar");
        assert!(
            !settings.editing(Field::User),
            "refusal restored a clean draft"
        );
        assert!(
            !settings.save(Field::User, |_| panic!("an error-present no-op was sent")),
            "an error does not turn an unchanged name into a change"
        );
        assert!(
            !settings.field(Field::User).saving,
            "the no-op stays unsent"
        );
        assert_eq!(
            settings.field(Field::User).error,
            Some(Error::Brain(ErrorKey::SettingsRefused.into()))
        );
    }

    #[test]
    fn save_uses_shared_validation_and_settles_the_trimmed_name() {
        let mut settings = connected();
        settings.change(Field::Assistant, "   ");
        assert!(
            !settings.save(Field::Assistant, |_| panic!("invalid name sent")),
            "an invalid name stays local"
        );
        assert_eq!(
            settings.field(Field::Assistant).error,
            Some(Error::Validation(NameError::Empty))
        );
        settings.change(Field::Assistant, "  Cloud  ");
        assert_eq!(settings.field(Field::Assistant).error, None);
        assert!(settings.editing(Field::Assistant), "the draft is dirty");
        assert!(
            settings.save(Field::Assistant, |op| {
                assert_eq!(
                    op,
                    SettingsOp::SetAssistantName(AssistantName::new("Cloud").expect("valid"))
                );
                true
            }),
            "a valid name is sent"
        );
        assert!(
            settings.field(Field::Assistant).saving,
            "the save is pending"
        );
        assert_eq!(settings.value(Field::Assistant), "  Cloud  ");
        assert!(
            !settings.save(Field::Assistant, |_| panic!("duplicate save")),
            "one field cannot have two saves in flight"
        );
        assert_eq!(
            settings.set_view(view("Cloud", Some("Oscar")), Update::Answer),
            Some(Completion {
                failed: false,
                field: Field::Assistant
            })
        );
        assert_eq!(settings.value(Field::Assistant), "Cloud");
        assert!(
            !settings.field(Field::Assistant).saving,
            "the answer settles it"
        );
        assert!(
            !settings.editing(Field::Assistant),
            "the saved name is clean"
        );
        assert_eq!(settings.view(), Some(&view("Cloud", Some("Oscar"))));
    }

    #[test]
    fn refusal_rolls_back_and_keeps_the_error_until_an_actual_change() {
        let mut settings = connected();
        settings.change(Field::Assistant, "Cloud");
        assert!(
            settings.save(Field::Assistant, |_| true),
            "the save is sent"
        );
        assert_eq!(
            settings.error(ErrorKey::SettingsRefused.into()),
            Some(Field::Assistant)
        );
        assert_eq!(
            settings.set_view(view("Kumo", Some("Oscar")), Update::Answer),
            Some(Completion {
                failed: true,
                field: Field::Assistant
            })
        );
        assert_eq!(settings.value(Field::Assistant), "Kumo");
        assert!(
            !settings.editing(Field::Assistant),
            "refusal restores the saved name"
        );
        let error = Some(Error::Brain(ErrorKey::SettingsRefused.into()));
        assert_eq!(settings.field(Field::Assistant).error, error);
        settings.change(Field::Assistant, "Kumo");
        settings.cancel(Field::Assistant);
        assert_eq!(settings.field(Field::Assistant).error, error);
        assert_eq!(
            settings.set_view(view("Kumo", Some("Oscar")), Update::Unsolicited),
            None
        );
        assert_eq!(settings.field(Field::Assistant).error, error);
        settings.change(Field::Assistant, "Cloud");
        assert_eq!(settings.field(Field::Assistant).error, None);
        settings.cancel(Field::Assistant);
        assert_eq!(settings.value(Field::Assistant), "Kumo");
        assert!(
            !settings.editing(Field::Assistant),
            "cancel restores the saved name"
        );
    }

    #[test]
    fn empty_or_whitespace_user_names_clear_the_setting() {
        for value in ["", " \t\n "] {
            let mut settings = connected();
            settings.change(Field::User, value);
            assert!(
                settings.save(Field::User, |op| {
                    assert_eq!(op, SettingsOp::SetUserName(None));
                    true
                }),
                "an empty user name sends a clear"
            );
            assert_eq!(
                settings.set_view(view("Kumo", None), Update::Answer),
                Some(Completion {
                    failed: false,
                    field: Field::User
                })
            );
            assert_eq!(settings.value(Field::User), "");
            assert!(!settings.editing(Field::User), "the cleared name is clean");
            assert_eq!(settings.field(Field::User).error, None);
        }
    }

    #[test]
    fn errors_and_answers_follow_both_fields_in_fifo_order() {
        let mut settings = connected();
        settings.change(Field::User, "Alex");
        assert!(
            settings.save(Field::User, |_| true),
            "the user save is sent first"
        );
        assert!(
            settings.send_assistant(AssistantName::new("Cloud").expect("valid"), |_| true),
            "the inline assistant rename is sent second"
        );
        assert_eq!(
            settings.error(ErrorKey::SettingsRefused.into()),
            Some(Field::User)
        );
        assert_eq!(settings.field(Field::Assistant).error, None);
        assert_eq!(
            settings.field(Field::User).error,
            Some(Error::Brain(ErrorKey::SettingsRefused.into()))
        );
        assert_eq!(
            settings.set_view(view("Kumo", Some("Oscar")), Update::Answer),
            Some(Completion {
                failed: true,
                field: Field::User
            })
        );
        assert!(
            !settings.field(Field::User).saving,
            "the first save is settled"
        );
        assert!(
            settings.field(Field::Assistant).saving,
            "the second save is pending"
        );
        assert_eq!(settings.value(Field::User), "Oscar");
        assert_eq!(
            settings.set_view(view("Cloud", Some("Oscar")), Update::Answer),
            Some(Completion {
                failed: false,
                field: Field::Assistant
            })
        );
        assert_eq!(settings.value(Field::Assistant), "Cloud");
        assert_eq!(
            settings.field(Field::User).error,
            Some(Error::Brain(ErrorKey::SettingsRefused.into()))
        );
        assert_eq!(
            settings.set_view(view("Cloud", Some("Oscar")), Update::Answer),
            None
        );
    }

    #[test]
    fn unsolicited_views_do_not_complete_saves_or_replace_drafts() {
        let mut settings = connected();
        settings.change(Field::Assistant, "Cloud");
        assert!(
            settings.save(Field::Assistant, |_| true),
            "the save is sent"
        );
        assert_eq!(
            settings.set_view(view("Other", Some("Alex")), Update::Unsolicited),
            None
        );
        assert!(
            settings.field(Field::Assistant).saving,
            "an unsolicited view is not an answer"
        );
        assert_eq!(settings.value(Field::Assistant), "Cloud");
        assert_eq!(settings.value(Field::User), "Alex");
        assert_eq!(
            settings.set_view(view("Cloud", Some("Alex")), Update::Answer),
            Some(Completion {
                failed: false,
                field: Field::Assistant
            })
        );
        assert!(
            !settings.field(Field::Assistant).saving,
            "the answer settles the save"
        );
    }

    #[test]
    fn later_typing_survives_success_and_refusal() {
        for failed in [false, true] {
            let mut settings = connected();
            settings.change(Field::Assistant, "Cloud");
            assert!(
                settings.save(Field::Assistant, |_| true),
                "the save is sent"
            );
            if failed {
                assert_eq!(
                    settings.error(ErrorKey::SettingsRefused.into()),
                    Some(Field::Assistant)
                );
            }
            settings.change(Field::Assistant, "Later");
            let saved = if failed { "Kumo" } else { "Cloud" };
            assert_eq!(
                settings.set_view(view(saved, Some("Oscar")), Update::Answer),
                Some(Completion {
                    failed,
                    field: Field::Assistant
                })
            );
            assert_eq!(settings.value(Field::Assistant), "Later");
            assert!(
                settings.editing(Field::Assistant),
                "later typing remains dirty"
            );
            assert_eq!(settings.field(Field::Assistant).error, None);
            settings.cancel(Field::Assistant);
            assert_eq!(settings.value(Field::Assistant), saved);
        }
    }

    #[test]
    fn reconnect_drops_old_fifo_and_preserves_errors() {
        let mut settings = connected();
        settings.change(Field::User, "Alex");
        assert!(
            settings.save(Field::User, |_| true),
            "the old connection accepted a save"
        );
        assert_eq!(
            settings.error(ErrorKey::ConfigUnreadable.into()),
            Some(Field::User)
        );
        settings.disconnected();
        assert_eq!(settings.view(), None);
        assert!(
            !settings.field(Field::User).saving,
            "disconnect drops the pending save"
        );
        assert_eq!(
            settings.field(Field::User).error,
            Some(Error::Brain(ErrorKey::ConfigUnreadable.into()))
        );
        settings.change(Field::Assistant, "OfflineDraft");
        assert!(
            !settings.save(Field::Assistant, |_| panic!("offline send")),
            "saving is disabled until settings arrive"
        );
        assert_eq!(settings.field(Field::Assistant).error, Some(Error::Offline));
        assert_eq!(
            settings.set_view(view("Kumo", Some("Oscar")), Update::Unsolicited),
            None
        );
        settings.change(Field::Assistant, "Cloud");
        assert!(
            settings.save(Field::Assistant, |_| true),
            "the new connection accepts a save"
        );
        assert_eq!(
            settings.set_view(view("Cloud", Some("Oscar")), Update::Answer),
            Some(Completion {
                failed: false,
                field: Field::Assistant
            })
        );
        assert_eq!(settings.value(Field::User), "Alex");
        assert_eq!(
            settings.field(Field::User).error,
            Some(Error::Brain(ErrorKey::ConfigUnreadable.into()))
        );
    }

    #[test]
    fn successful_retry_clears_only_its_brain_error() {
        let mut settings = connected();
        assert!(
            settings.send_assistant(AssistantName::new("Cloud").expect("valid"), |_| true),
            "the first rename is sent"
        );
        assert_eq!(
            settings.error(ErrorKey::SettingsRefused.into()),
            Some(Field::Assistant)
        );
        assert_eq!(
            settings.set_view(view("Kumo", Some("Oscar")), Update::Answer),
            Some(Completion {
                failed: true,
                field: Field::Assistant
            })
        );
        assert_eq!(settings.value(Field::Assistant), "Kumo");
        assert!(
            !settings.send_assistant(AssistantName::new("Kumo").expect("valid"), |_| panic!(
                "an error-present inline no-op was sent"
            )),
            "the error does not make the saved assistant name a change"
        );
        settings.change(Field::User, "Alex");
        assert!(
            !settings.save(Field::User, |_| false),
            "a rejected send is not queued"
        );
        assert!(
            settings.send_assistant(AssistantName::new("Cloud").expect("valid"), |_| true),
            "the failed different name is retried without editing the settings draft"
        );
        assert_eq!(settings.value(Field::Assistant), "Kumo");
        assert_eq!(
            settings.field(Field::Assistant).error,
            Some(Error::Brain(ErrorKey::SettingsRefused.into()))
        );
        assert_eq!(
            settings.set_view(view("Cloud", Some("Oscar")), Update::Answer),
            Some(Completion {
                failed: false,
                field: Field::Assistant
            })
        );
        assert_eq!(settings.field(Field::Assistant).error, None);
        assert_eq!(settings.field(Field::User).error, Some(Error::Offline));
        assert!(
            !settings.field(Field::User).saving,
            "a rejected send is never pending"
        );
    }

    #[test]
    fn successful_retry_of_the_same_draft_clears_its_offline_error() {
        let mut settings = connected();
        settings.change(Field::Assistant, "Cloud");
        assert!(
            !settings.save(Field::Assistant, |_| false),
            "the first send fails"
        );
        assert_eq!(settings.value(Field::Assistant), "Cloud");
        assert_eq!(settings.field(Field::Assistant).error, Some(Error::Offline));
        assert!(
            !settings.field(Field::Assistant).saving,
            "a failed send is not pending"
        );
        settings.disconnected();
        assert_eq!(settings.view(), None);
        assert_eq!(
            settings.set_view(view("Kumo", Some("Oscar")), Update::Unsolicited),
            None
        );
        assert_eq!(settings.value(Field::Assistant), "Cloud");
        assert_eq!(settings.field(Field::Assistant).error, Some(Error::Offline));
        assert!(
            settings.save(Field::Assistant, |_| true),
            "the same dirty draft is retried after reconnect"
        );
        assert!(
            settings.field(Field::Assistant).saving,
            "the retry is pending"
        );
        assert_eq!(settings.field(Field::Assistant).error, Some(Error::Offline));
        assert_eq!(
            settings.set_view(view("Cloud", Some("Oscar")), Update::Answer),
            Some(Completion {
                failed: false,
                field: Field::Assistant
            })
        );
        assert_eq!(settings.value(Field::Assistant), "Cloud");
        assert_eq!(settings.field(Field::Assistant).error, None);
        assert!(
            !settings.field(Field::Assistant).saving,
            "success settles the retry"
        );
        assert!(
            !settings.editing(Field::Assistant),
            "the saved draft is clean"
        );
    }

    #[test]
    fn successful_inline_rename_preserves_an_invalid_settings_draft() {
        let mut settings = connected();
        settings.change(Field::Assistant, "");
        assert!(
            !settings.save(Field::Assistant, |_| panic!("invalid draft sent")),
            "the invalid settings draft stays local"
        );
        assert!(
            settings.send_assistant(AssistantName::new("Cloud").expect("valid"), |_| true),
            "the independent inline rename is sent"
        );
        assert_eq!(
            settings.set_view(view("Cloud", Some("Oscar")), Update::Answer),
            Some(Completion {
                failed: false,
                field: Field::Assistant
            })
        );
        assert_eq!(settings.value(Field::Assistant), "");
        assert_eq!(
            settings.field(Field::Assistant).error,
            Some(Error::Validation(NameError::Empty))
        );
        assert!(
            settings.editing(Field::Assistant),
            "the local draft remains dirty"
        );
        assert!(
            !settings.field(Field::Assistant).saving,
            "the inline rename settled"
        );
    }

    #[test]
    fn inline_rename_preserves_an_existing_settings_draft() {
        let mut settings = connected();
        settings.change(Field::Assistant, "Draft");
        assert!(
            settings.send_assistant(AssistantName::new("Cloud").expect("valid"), |_| true),
            "the inline rename is sent"
        );
        assert_eq!(
            settings.set_view(view("Cloud", Some("Oscar")), Update::Answer),
            Some(Completion {
                failed: false,
                field: Field::Assistant
            })
        );
        assert_eq!(settings.value(Field::Assistant), "Draft");
        settings.cancel(Field::Assistant);
        assert_eq!(settings.value(Field::Assistant), "Cloud");
    }

    #[test]
    fn debug_never_contains_names_or_drafts() {
        let mut settings = Settings::default();
        assert_eq!(
            settings.set_view(
                view("AssistantSentinel", Some("UserSentinel")),
                Update::Unsolicited
            ),
            None
        );
        settings.change(Field::Assistant, "DraftSentinel");
        assert!(
            settings.save(Field::Assistant, |_| true),
            "the draft is also queued"
        );
        let debug = format!("{settings:?} {:?}", settings.field(Field::Assistant));
        assert!(
            !debug.contains("AssistantSentinel"),
            "the saved assistant name is redacted"
        );
        assert!(
            !debug.contains("UserSentinel"),
            "the saved user name is redacted"
        );
        assert!(!debug.contains("DraftSentinel"), "the draft is redacted");
    }
}
