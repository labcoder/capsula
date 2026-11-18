//! The dormant Anthropic key row in Accounts.

use std::time::Instant;

use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Disableable as _};
use gpui_kit::{
    ClipboardEntry, ClipboardItem, Context, InteractiveElement as _, IntoElement,
    ParentElement as _, Styled as _, Window, div,
};
use kumochi_protocol::message::{
    ConnectionOp, ControlToBrain, ControlToShell, KeyRejection, KeyView,
};
use kumochi_shell_core::key_paste::{KeyPaste, PasteFailure, Removal};
use zeroize::Zeroizing;

use super::SettingsView;
use crate::presence::Presence;
use crate::strings::{Strings, Text};

impl SettingsView {
    pub(super) fn anthropic(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let state = &self.model.read(cx).key_paste;
        let paste = Button::new("anthropic-paste")
            .debug_selector(|| "anthropic-paste".into())
            .label(strings.get(Text::AnthropicPaste))
            .disabled(!state.can_paste())
            .on_click(cx.listener(|view, _, _, cx| view.paste_key(cx)));
        let retry = state.check_available().then(|| {
            let label = if state.saved() && state.removal() == Removal::None {
                Text::AnthropicCheck
            } else {
                Text::AnthropicRetry
            };
            Button::new("anthropic-check")
                .debug_selector(|| "anthropic-check".into())
                .outline()
                .label(strings.get(label))
                .on_click(cx.listener(|view, _, _, cx| {
                    let _gone = view.owner.update(cx, |owner, cx| {
                        owner.with_key_session(cx, |state, _, send| state.check(send));
                    });
                }))
        });
        let remove = state.remove_revision().map(|_| {
            Button::new("anthropic-remove")
                .debug_selector(|| "anthropic-remove".into())
                .outline()
                .label(strings.get(Text::AnthropicRemoveOpen))
                .on_click(cx.listener(|view, _, window, cx| view.confirm_remove_key(window, cx)))
        });
        div()
            .id("anthropic-accounts")
            .flex()
            .flex_col()
            .gap_3()
            .min_w_0()
            .border_t_1()
            .border_color(cx.theme().border)
            .pt_4()
            .child(div().text_lg().child(strings.get(Text::AnthropicTitle)))
            .children(
                [
                    Text::AnthropicIntro,
                    Text::AnthropicDisclosure,
                    Text::AnthropicBilling,
                    status(state),
                ]
                .into_iter()
                .map(|text| div().text_sm().child(strings.get(text))),
            )
            .children(
                (state.saved()
                    && state.removal() == Removal::None
                    && matches!(state.view(), Some(KeyView::Checking | KeyView::Unconfirmed)))
                .then(|| div().text_sm().child(strings.get(Text::AnthropicSaved))),
            )
            .children((state.can_paste() && state.view() != Some(KeyView::Missing)).then(|| {
                let text = strings.get(Text::AnthropicReplaceNote);
                div().debug_selector(|| text.clone()).text_sm().child(text)
            }))
            .children(notice(state).map(|text| div().text_sm().child(strings.get(text))))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(paste)
                    .children(retry)
                    .children(remove),
            )
    }

    fn paste_key(&self, cx: &mut Context<'_, Self>) {
        if !self.model.read(cx).key_paste.can_paste() {
            return;
        }
        // shortcut: GPUI materializes the clipboard and may log an unsupported format's name
        // before we bound retained text; revisit with NativeShell.
        let text = clipboard_text(cx.read_from_clipboard());
        let _gone = self
            .owner
            .update(cx, |owner, cx| owner.begin_key_paste(text, cx));
    }

    fn confirm_remove_key(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(revision) = self.model.read(cx).key_paste.remove_revision() else {
            return;
        };
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        let Some(session) = owner
            .read(cx)
            .session
            .as_ref()
            .map(crate::link::Session::generation)
        else {
            return;
        };
        let owner = self.owner.clone();
        let strings = cx.global::<Strings>();
        let [title, body, cancel, remove] = [
            Text::AnthropicRemoveTitle,
            Text::AnthropicRemoveBody,
            Text::AccountDialogCancel,
            Text::AnthropicRemove,
        ]
        .map(|text| strings.get(text).into());
        let _focus =
            crate::confirmation::open(window, cx, title, body, cancel, remove, move |_, cx| {
                let _gone = owner.update(cx, |owner, cx| {
                    owner.with_key_session(cx, |state, current, send| {
                        if current == session {
                            state.remove(revision, send);
                        }
                    });
                });
            });
    }
}

fn clipboard_text(item: Option<ClipboardItem>) -> Option<Zeroizing<String>> {
    let item = item?;
    let single = item.entries().len() == 1;
    let mut value = None;
    for entry in item.into_entries() {
        if let ClipboardEntry::String(text) = entry {
            let text = Zeroizing::new(text.into_text());
            if single {
                value = Some(text);
            }
        }
    }
    value
}

fn status(state: &KeyPaste) -> Text {
    match state.removal() {
        Removal::Waiting => return Text::AnthropicRemoving,
        Removal::Retry => return Text::AnthropicRemovePending,
        Removal::None => {}
    }
    if state.saving() {
        return Text::AnthropicSaving;
    }
    match state.view() {
        None => Text::Failed,
        Some(KeyView::Missing) => Text::AnthropicMissing,
        Some(KeyView::Checking) => Text::AnthropicChecking,
        Some(KeyView::Ready) => Text::AnthropicReady,
        Some(KeyView::Rejected(_)) => Text::AnthropicRejected,
        Some(KeyView::Unconfirmed) => Text::AnthropicUnconfirmed,
    }
}

fn notice(state: &KeyPaste) -> Option<Text> {
    match state.failure() {
        Some(PasteFailure::Clipboard) => Some(Text::AnthropicClipboardInvalid),
        Some(PasteFailure::TooLong) => Some(Text::AnthropicClipboardTooLong),
        Some(PasteFailure::Unsaved) => Some(Text::AnthropicSaveFailed),
        Some(PasteFailure::Unconfirmed) => Some(Text::AnthropicSaveUnconfirmed),
        Some(PasteFailure::Offline) => Some(Text::AccountSendFailed),
        Some(PasteFailure::Rejected(reason)) => Some(rejection(reason)),
        None => match state.view() {
            Some(KeyView::Rejected(reason)) => Some(rejection(reason)),
            _ => None,
        },
    }
}

fn rejection(reason: KeyRejection) -> Text {
    match reason {
        KeyRejection::Format => Text::AnthropicFormat,
        KeyRejection::SubscriptionToken => Text::AnthropicSubscriptionToken,
        KeyRejection::AdminKey => Text::AnthropicAdminKey,
        KeyRejection::WorkspaceRequired => Text::AnthropicWorkspaceRequired,
        KeyRejection::Refused => Text::AnthropicRefused,
        KeyRejection::Credit | KeyRejection::RateLimited => Text::AnthropicUnconfirmed,
    }
}

impl Presence {
    fn with_key_session<T>(
        &self,
        cx: &mut Context<'_, Self>,
        update: impl FnOnce(&mut KeyPaste, u64, &dyn Fn(ConnectionOp) -> bool) -> T,
    ) -> Option<T> {
        let session = self.session.as_ref()?;
        Some(self.settings.update(cx, |state, cx| {
            let send = |op| {
                self.link
                    .send_control(session, ControlToBrain::Connection(op))
            };
            let result = update(&mut state.key_paste, session.generation(), &send);
            cx.notify();
            result
        }))
    }

    fn begin_key_paste(&self, text: Option<Zeroizing<String>>, cx: &mut Context<'_, Self>) {
        let at = self
            .with_key_session(cx, |state, session, send| {
                state.begin(text, session, Instant::now(), send)
            })
            .flatten();
        if let Some(at) = at {
            cx.spawn(async move |owner, cx| {
                cx.background_executor()
                    .timer(at.deadline.saturating_duration_since(Instant::now()))
                    .await;
                let _gone = owner.update(cx, |owner, cx| {
                    owner.settings.update(cx, |state, cx| {
                        if state.key_paste.expire(at, Instant::now()) {
                            cx.notify();
                        }
                    });
                });
            })
            .detach();
        }
    }

    pub(in crate::presence) fn key_stage(
        &self,
        generation: u64,
        reply: &ControlToShell,
        cx: &mut Context<'_, Self>,
    ) {
        if self
            .session
            .as_ref()
            .is_none_or(|session| session.generation() != generation)
        {
            return;
        }
        let job = self.settings.update(cx, |state, cx| {
            let job = match reply {
                ControlToShell::AnthropicKeyStageDone { .. } => {
                    state.key_paste.done(generation, reply, Instant::now());
                    None
                }
                _ => state.key_paste.planned(generation, reply, Instant::now()),
            };
            cx.notify();
            job
        });
        if let Some(job) = job {
            let writer = cx.background_executor().spawn(async move { job.run() });
            cx.spawn(async move |owner, cx| {
                let result = writer.await;
                let _gone = owner.update(cx, |owner, cx| {
                    owner.with_key_session(cx, |state, session, send| {
                        if session == generation {
                            state.written(&result, Instant::now(), send);
                        }
                    });
                });
            })
            .detach();
        }
    }
}

#[cfg(test)]
mod tests;
