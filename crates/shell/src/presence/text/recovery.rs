//! Declared recovery controls and their owner-checked dispatch.

#[cfg(test)]
mod tests;

use super::{Context, OpenError, Presence, TextView, submission_notice};
use crate::chat_notice::{Route, Source};
use crate::runs::Refused;
use crate::strings::{Strings, Text};
use gpui_kit::base::Link;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _,
    button::{Button, ButtonVariants as _},
};
use gpui_kit::{InteractiveElement as _, IntoElement as _, ParentElement as _, Styled as _, div};
use kumochi_protocol::message::{Action, MANAGE_USAGE_URL};

impl Presence {
    fn dismiss_recovery(&mut self, source: Source, cx: &mut Context<'_, Self>) {
        if self
            .chat_notice
            .shown()
            .is_none_or(|notice| notice.source != source)
        {
            return;
        }
        self.dismiss_recovery_notice(cx);
        self.runs.dismiss_retry();
        self.refresh_conversation(cx);
    }

    fn recover(
        &mut self,
        source: Source,
        action: Action,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let Some(route) = self
            .chat_notice
            .shown()
            .filter(|notice| notice.source == source && notice.error.actions().contains(&action))
            .and_then(|notice| notice.route(action))
        else {
            return Ok(());
        };
        match route {
            Route::RunRetry(run) => {
                self.cancel_voice(cx);
                self.dismiss_recovery_notice(cx);
                let Some(input) = self.text.as_ref().map(|text| text.input.clone()) else {
                    self.runs.dismiss_retry();
                    return Ok(());
                };
                let draft = input.read(cx).value();
                let (link, session) = (&self.link, &self.session);
                let result = if self.settings.read(cx).accounts.waiting.is_some() {
                    self.runs.dismiss_retry();
                    Err(Refused::Busy)
                } else {
                    self.runs.retry(run, &draft, |turn| {
                        session
                            .as_ref()
                            .is_some_and(|session| link.send_data(session, turn))
                    })
                };
                let previous = (self.error.clone(), self.refusal);
                submission_notice(&mut self.error, &mut self.refusal, result);
                if result.is_ok() {
                    self.chat_notice.clear();
                }
                self.notices_changed(&previous, cx);
                self.runs_changed(cx)?;
            }
            Route::Account(op) => {
                let _sent = self.account_op(op, cx);
            }
            Route::SettingsRetry(field) => {
                let _sent = self.save_setting(field, cx);
            }
            Route::Accounts => self.open_accounts(cx)?,
            Route::Settings => self.open_general(cx)?,
            Route::Usage => cx.open_url(MANAGE_USAGE_URL),
        }
        Ok(())
    }

    fn dismiss_recovery_notice(&mut self, cx: &mut Context<'_, Self>) {
        self.chat_notice.dismiss();
        self.settings.update(cx, |state, _| state.clear_notice());
        self.sync_settings_notice(cx);
    }
}

impl TextView {
    #[expect(
        clippy::too_many_lines,
        reason = "one notice keeps each control beside its deferred dispatch"
    )]
    pub(super) fn recovery(&self, cx: &Context<'_, Self>) -> Option<gpui_kit::AnyElement> {
        let notice = self.recovery.as_ref().filter(|_| self.refusal.is_none())?;
        let strings = cx.global::<Strings>();
        let settings = self.settings.read(cx);
        let connected = self.model.read(cx).online();
        let source = notice.source;
        let actions = notice
            .error
            .actions()
            .iter()
            .copied()
            .enumerate()
            .map(|(ix, action)| {
                let route = notice.route(action);
                let enabled = match route {
                    Some(Route::RunRetry(_)) => {
                        connected
                            && self.retry
                            && self.draft_empty
                            && settings.accounts.waiting.is_none()
                    }
                    Some(Route::Account(op)) => connected && settings.accounts.allows(op),
                    Some(Route::SettingsRetry(field)) => connected && !settings.field(field).saving,
                    Some(Route::Accounts | Route::Settings | Route::Usage) => true,
                    None => false,
                };
                let owner = self.owner.clone();
                let input = self.input.clone();
                if route == Some(Route::Usage) {
                    return Link::new(("chat-recovery", ix))
                        .debug_selector(move || format!("chat-recovery-action-{ix}"))
                        .href(MANAGE_USAGE_URL)
                        .text_color(cx.theme().link)
                        .underline()
                        .open_with(move |_, _, _, cx| {
                            if let Ok(Err(_)) =
                                owner.update(cx, |this, cx| this.recover(source, action, cx))
                            {
                                tracing::warn!("can't open chat recovery");
                            }
                        })
                        .child(strings.get(Text::AccountManageUsage))
                        .into_any_element();
                }
                let label = match action {
                    Action::Retry => Text::Retry,
                    Action::SignIn if matches!(route, Some(Route::Account(_))) => {
                        Text::RecoverySignIn
                    }
                    Action::SignIn | Action::OpenSettings => Text::OpenSettings,
                    Action::Enable => Text::AccountEnable,
                    Action::ManageUsage => Text::AccountManageUsage,
                };
                Button::new(("chat-recovery", ix))
                    .debug_selector(move || format!("chat-recovery-action-{ix}"))
                    .label(strings.get(label))
                    .disabled(!enabled)
                    .on_click(move |_, window, cx| {
                        let owner = owner.clone();
                        // Settings hides this window, which GPUI can't update mid-click.
                        cx.defer(move |cx| {
                            if let Ok(Err(_)) =
                                owner.update(cx, |this, cx| this.recover(source, action, cx))
                            {
                                tracing::warn!("can't apply chat recovery");
                            }
                        });
                        if action == Action::Retry {
                            input.update(cx, |input, cx| input.focus(window, cx));
                        }
                    })
                    .into_any_element()
            });
        let owner = self.owner.clone();
        let input = self.input.clone();
        Some(
            div()
                .debug_selector(|| "chat-recovery".into())
                .flex()
                .flex_col()
                .gap_2()
                .child(div().text_sm().children(self.recovery_text.clone()))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .children(actions)
                        .child(
                            Button::new("chat-recovery-dismiss")
                                .debug_selector(|| "chat-recovery-dismiss".into())
                                .ghost()
                                .label(strings.get(Text::DismissNotice))
                                .on_click(move |_, window, cx| {
                                    let _gone = owner
                                        .update(cx, |this, cx| this.dismiss_recovery(source, cx));
                                    input.update(cx, |input, cx| input.focus(window, cx));
                                }),
                        ),
                )
                .into_any_element(),
        )
    }
}
