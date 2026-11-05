//! Accounts composes the brain's snapshot; controls only request operations.

use gpui_kit::assets::IconName;
use gpui_kit::base::Link;
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _};
use gpui_kit::{
    App, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    Styled as _, Window, div,
};
use kumochi_protocol::message::{
    AccountOp, AccountRecovery, AccountView, Action, Cleanup, ErrorKey, MANAGE_USAGE_URL, PlanState,
};

use super::SettingsView;
use crate::accounts::Accounts;
use crate::strings::{Strings, Text};

impl SettingsView {
    fn account_button(
        &self,
        id: &'static str,
        label: Text,
        op: AccountOp,
        cx: &Context<'_, Self>,
    ) -> Button {
        let owner = self.owner.clone();
        Button::new(id)
            .label(cx.global::<Strings>().get(label))
            .disabled(!self.model.read(cx).accounts.allows(op))
            .on_click(move |_, _, cx| {
                let _gone = owner.update(cx, |owner, cx| owner.account_op(op, cx));
            })
    }

    fn account_actions(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let state = &self.model.read(cx).accounts;
        let mut actions = div().flex().flex_wrap().items_center().gap_2();
        match &state.view {
            Some(AccountView::SignedOut { .. }) => {
                actions = actions.child(self.account_button(
                    "account-sign-in",
                    Text::AccountContinue,
                    AccountOp::SignIn,
                    cx,
                ));
            }
            Some(AccountView::SigningIn) => {
                actions = actions
                    .child(self.account_button(
                        "account-cancel",
                        Text::AccountCancel,
                        AccountOp::CancelSignIn,
                        cx,
                    ))
                    .child(self.account_button(
                        "account-open-again",
                        Text::AccountOpenAgain,
                        AccountOp::SignIn,
                        cx,
                    ));
            }
            Some(AccountView::SignedIn { recovery, .. })
                if *recovery == Some(AccountRecovery::SignInAgain)
                    || state
                        .error
                        .as_ref()
                        .is_some_and(|error| error.actions().contains(&Action::SignIn)) =>
            {
                actions = actions
                    .debug_selector(|| "account-sign-in-recovery".into())
                    .child(self.account_button(
                        "account-sign-in-again",
                        Text::AccountSignInAgain,
                        AccountOp::SignIn,
                        cx,
                    ));
            }
            Some(AccountView::SignedIn {
                plan: PlanState::Declined,
                recovery: None,
                ..
            }) => {
                actions = actions.child(self.account_button(
                    "account-enable",
                    Text::AccountEnable,
                    AccountOp::EnablePlan,
                    cx,
                ));
            }
            _ => {}
        }
        if state.allows(AccountOp::Retry) || state.waiting == Some(AccountOp::Retry) {
            actions = actions.child(self.account_button(
                "account-retry",
                if matches!(state.view, Some(AccountView::Busy)) {
                    Text::AccountTryAgain
                } else {
                    Text::Retry
                },
                AccountOp::Retry,
                cx,
            ));
        }
        if matches!(
            state.view,
            Some(AccountView::SignedIn {
                plan: PlanState::Enabled | PlanState::Unavailable,
                recovery: None,
                ..
            })
        ) {
            #[cfg(debug_assertions)]
            let fixture = self.fixture;
            actions = actions.child(
                Link::new("account-usage")
                    .href(MANAGE_USAGE_URL)
                    .disabled(state.waiting.is_some())
                    .open_with(move |url, _, _, cx| {
                        #[cfg(debug_assertions)]
                        if fixture {
                            return;
                        }
                        cx.open_url(url);
                    })
                    .text_color(cx.theme().link)
                    .underline()
                    .child(strings.get(Text::AccountManageUsage)),
            );
        }
        actions
    }

    pub(super) fn accounts(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let model = self.model.read(cx);
        let state = &model.accounts;
        let (mut status, lines) = account_status(state);
        if state.view.is_none() && model.view().is_none() {
            status = Text::AccountOffline;
        }
        let duplicate = state.error.as_ref().is_some_and(|error| {
            matches!(
                (error.key(), &state.view),
                (
                    ErrorKey::SignOutNotDurable,
                    Some(AccountView::SigningOut {
                        cleanup: Cleanup::NotDurable,
                        ..
                    })
                ) | (
                    ErrorKey::SessionEnded,
                    Some(AccountView::SignedIn {
                        recovery: Some(AccountRecovery::SignInAgain),
                        ..
                    })
                ) | (
                    ErrorKey::PlanUnavailable,
                    Some(AccountView::SignedIn {
                        plan: PlanState::Unavailable,
                        recovery: None,
                        ..
                    })
                )
            )
        });
        div()
            .flex()
            .flex_col()
            .gap_6()
            .min_w_0()
            .child(account_header(state, cx))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(strings.get(status)),
            )
            .children(lines.into_iter().map(|text| account_notice(text, cx)))
            .children(
                state
                    .error
                    .as_ref()
                    .filter(|_| !duplicate)
                    .map(|error| account_error(error, cx)),
            )
            .children(state.send_failed.then(|| {
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(strings.get(Text::AccountSendFailed))
            }))
            .child(self.account_actions(cx))
            .children(
                matches!(state.view, Some(AccountView::SignedIn { .. })).then(|| {
                    div()
                        .flex()
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .pt_4()
                        .child(
                            Button::new("account-sign-out")
                                .outline()
                                .label(strings.get(Text::AccountSignOut))
                                .disabled(!state.allows(AccountOp::SignOut))
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.confirm_sign_out(window, cx);
                                })),
                        )
                }),
            )
    }

    pub(in crate::presence) fn confirm_sign_out(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        let state = &self.model.read(cx).accounts;
        if !state.allows(AccountOp::SignOut) {
            return;
        }
        let Some(generation) = state.generation else {
            return;
        };
        let strings = cx.global::<Strings>();
        let mut body = strings.get(Text::AccountSignOutBody);
        if matches!(
            state.view,
            Some(AccountView::SignedIn {
                plan: PlanState::Unavailable,
                ..
            })
        ) {
            body.push_str("\n\n");
            body.push_str(&strings.get(Text::AccountSignOutRegistration));
        }
        let model = self.model.clone();
        let owner = self.owner.clone();
        let title = strings.get(Text::AccountSignOutTitle).into();
        let cancel = strings.get(Text::AccountDialogCancel).into();
        let confirm = strings.get(Text::AccountSignOutConfirm).into();
        self.confirmation = Some(generation);
        let _focus = crate::confirmation::open(
            window,
            cx,
            title,
            body.into(),
            cancel,
            confirm,
            move |_, cx: &mut App| {
                if model.read(cx).accounts.generation == Some(generation) {
                    let _gone =
                        owner.update(cx, |owner, cx| owner.account_op(AccountOp::SignOut, cx));
                }
            },
        );
    }
}

fn account_header(state: &Accounts, cx: &App) -> impl IntoElement {
    let strings = cx.global::<Strings>();
    let identity = match &state.view {
        Some(AccountView::SignedIn { email, .. }) => Some(email.as_ref().map_or_else(
            || strings.get(Text::AccountIdentity),
            |email| email.as_str().to_owned(),
        )),
        _ => None,
    };
    div()
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .p_2()
                .rounded_md()
                .bg(cx.theme().muted)
                .text_lg()
                .child(IconName::Bot),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(strings.get(Text::AccountProvider)),
                )
                .children(identity.map(|identity| {
                    div()
                        .text_base()
                        .font_weight(FontWeight::MEDIUM)
                        .child(identity)
                })),
        )
}

fn account_error(error: &kumochi_protocol::message::UserError, cx: &App) -> impl IntoElement {
    let strings = cx.global::<Strings>();
    if error.key() == ErrorKey::RevocationBacklog {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .max_w(gpui_kit::rems(36.))
            .text_sm()
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(cx.theme().danger)
                    .child(strings.get(Text::AccountBacklogLead)),
            )
            .children(
                [
                    Text::AccountBacklogRetry,
                    Text::AccountBacklogExpiry,
                    Text::AccountBacklogRecovery,
                ]
                .map(|text| {
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child(strings.get(text))
                }),
            )
    } else {
        div()
            .text_sm()
            .text_color(cx.theme().danger)
            .child(strings.error(error))
    }
}

fn account_notice(text: Text, cx: &App) -> impl IntoElement {
    let strings = cx.global::<Strings>();
    let mut notice = div().flex().flex_col().gap_2().text_sm();
    if matches!(text, Text::AccountCleanupRefused | Text::AccountNotDurable) {
        notice = notice
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .child(strings.get(Text::AccountLocal)),
            )
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(cx.theme().danger)
                    .child(strings.get(text)),
            )
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(strings.get(if text == Text::AccountCleanupRefused {
                        Text::AccountCleanupRefusedHelp
                    } else {
                        Text::AccountNotDurableHelp
                    })),
            );
    } else if matches!(
        text,
        Text::AccountRevocationPending | Text::AccountRevocationUnconfirmed
    ) {
        notice = notice
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .child(strings.get(Text::AccountRemote)),
            )
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(strings.get(text)),
            );
    } else {
        notice = notice
            .text_color(cx.theme().muted_foreground)
            .child(strings.get(text));
    }
    notice
}

fn account_status(state: &Accounts) -> (Text, Vec<Text>) {
    let mut lines = Vec::new();
    let status = match &state.view {
        None => Text::AccountWaiting,
        Some(AccountView::SignedOut {
            revocation_unconfirmed,
        }) => {
            if *revocation_unconfirmed {
                lines.push(Text::AccountRevocationUnconfirmed);
            } else {
                lines.push(Text::AccountUsePlan);
            }
            Text::AccountSignedOut
        }
        Some(AccountView::SigningIn) => {
            lines.push(Text::AccountReplaceHelp);
            Text::AccountSigningIn
        }
        Some(AccountView::Busy) => {
            lines.push(Text::AccountBusyHelp);
            Text::AccountBusy
        }
        Some(view @ AccountView::SigningOut { cleanup, .. }) => {
            lines.push(match cleanup {
                Cleanup::Running => Text::AccountStopped,
                Cleanup::Refused => Text::AccountCleanupRefused,
                Cleanup::NotDurable => Text::AccountNotDurable,
            });
            lines.extend(cleanup_notice(view));
            if *cleanup == Cleanup::Running {
                Text::AccountSigningOut
            } else {
                Text::AccountSignOutNeedsRetry
            }
        }
        Some(AccountView::SignedIn { plan, recovery, .. }) => match recovery {
            Some(AccountRecovery::SignInAgain) => Text::AccountSessionEnded,
            Some(AccountRecovery::Retry) => {
                if state.error.is_none() {
                    lines.push(Text::AccountRetryHelp);
                }
                Text::AccountAttention
            }
            None => match plan {
                PlanState::Enabled => Text::AccountUsingPlan,
                PlanState::Declined => Text::AccountPlanOff,
                PlanState::Unavailable => Text::AccountUnavailable,
            },
        },
    };
    (status, lines)
}

fn cleanup_notice(view: &AccountView) -> Option<Text> {
    matches!(
        view,
        AccountView::SigningOut {
            cleanup: Cleanup::Refused | Cleanup::NotDurable,
            revocation_unconfirmed: true,
        }
    )
    .then_some(Text::AccountRevocationPending)
}

#[cfg(test)]
mod tests {
    use super::{AccountView, Accounts, Cleanup, Text, account_status, cleanup_notice};

    #[test]
    fn remote_sign_out_copy_follows_the_cleanup_phase() {
        for (cleanup, remote, notice, lines) in [
            (Cleanup::Running, false, None, vec![Text::AccountStopped]),
            (Cleanup::Running, true, None, vec![Text::AccountStopped]),
            (
                Cleanup::Refused,
                false,
                None,
                vec![Text::AccountCleanupRefused],
            ),
            (
                Cleanup::Refused,
                true,
                Some(Text::AccountRevocationPending),
                vec![Text::AccountCleanupRefused, Text::AccountRevocationPending],
            ),
            (
                Cleanup::NotDurable,
                false,
                None,
                vec![Text::AccountNotDurable],
            ),
            (
                Cleanup::NotDurable,
                true,
                Some(Text::AccountRevocationPending),
                vec![Text::AccountNotDurable, Text::AccountRevocationPending],
            ),
        ] {
            let view = AccountView::SigningOut {
                cleanup,
                revocation_unconfirmed: remote,
            };
            assert_eq!(cleanup_notice(&view), notice, "{view:?}");
            let mut state = Accounts::default();
            assert!(state.receive(Some(7), 7, view));
            assert_eq!(
                account_status(&state),
                (
                    if cleanup == Cleanup::Running {
                        Text::AccountSigningOut
                    } else {
                        Text::AccountSignOutNeedsRetry
                    },
                    lines
                )
            );
        }
    }
}
