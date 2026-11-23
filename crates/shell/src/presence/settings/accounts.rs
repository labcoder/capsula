//! Accounts composes the brain's snapshot; controls only request operations.

use gpui_kit::assets::IconName;
use gpui_kit::base::Link;
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _};
use gpui_kit::{
    AnyElement, App, Context, Div, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Styled as _, Window, div,
};
use kumochi_protocol::message::{
    AccountOp, AccountRecovery, AccountView, Cleanup, ErrorKey, MANAGE_USAGE_URL, PlanState,
};

use super::SettingsView;
use crate::accounts::Accounts;
use crate::settings::Settings;
use crate::strings::{Strings, Text};

impl SettingsView {
    pub(super) fn accounts(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let owner = self.owner.clone();
        #[cfg(debug_assertions)]
        let fixture = self.fixture;
        #[cfg(not(debug_assertions))]
        let fixture = false;
        let model = self.model.read(cx);
        let sign_out = Button::new("account-sign-out")
            .debug_selector(|| "account-sign-out".into())
            .outline()
            .label(cx.global::<Strings>().get(Text::AccountSignOut))
            .disabled(!model.accounts.allows(AccountOp::SignOut))
            .on_click(cx.listener(|view, _, window, cx| {
                view.confirm_sign_out(window, cx);
            }));
        account_section(
            model,
            move |op, cx| {
                let _gone = owner.update(cx, |owner, cx| owner.settings_account_op(op, cx));
            },
            fixture,
            ProviderLayout::Settings(sign_out.into_any_element()),
            cx,
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
        let view = cx.weak_entity();
        let title = strings.get(Text::AccountSignOutTitle).into();
        let cancel = strings.get(Text::AccountDialogCancel).into();
        let confirm = strings.get(Text::AccountSignOutConfirm).into();
        let dialog = crate::confirmation::open_during_exit(
            window,
            cx,
            title,
            body.into(),
            cancel,
            confirm,
            move |confirmed, _, cx| {
                let _gone = view.update(cx, |view, cx| {
                    if view
                        .confirmation
                        .as_ref()
                        .is_none_or(|(current, _)| *current != generation)
                    {
                        return;
                    }
                    view.confirmation = None;
                    if confirmed && view.model.read(cx).accounts.generation == Some(generation) {
                        let _gone = view
                            .owner
                            .update(cx, |owner, cx| owner.account_op(AccountOp::SignOut, cx));
                    }
                });
            },
        );
        if let Some(dialog) = dialog {
            self.confirmation = Some((generation, dialog));
        }
    }
}

pub(in crate::presence) enum ProviderLayout<'a> {
    Settings(AnyElement),
    Onboarding(&'a mut Vec<Div>),
}

pub(super) fn provider_row(title: Div, status: Div, actions: Div) -> Div {
    div()
        .child(
            div()
                .flex()
                .items_start()
                .justify_between()
                .gap_2()
                .child(title.min_w_0())
                .child(actions.flex_shrink_0()),
        )
        .child(status)
}

/// The `ChatGPT` section, for every view that shows the account. `route` sends a button's
/// operation; `fixture` keeps Manage usage inert.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "Settings' debug fixture flag, passed through"
)]
pub(in crate::presence) fn account_section(
    model: &Settings,
    route: impl Fn(AccountOp, &mut App) + Clone + 'static,
    fixture: bool,
    layout: ProviderLayout<'_>,
    cx: &App,
) -> Div {
    let strings = cx.global::<Strings>();
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
    let header = account_header(state, cx);
    let status = div()
        .debug_selector(|| "account-status".into())
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .child(strings.get(status));
    let actions = account_actions(state, route, fixture, cx);
    let details = lines
        .into_iter()
        .map(|text| account_notice(text, cx))
        .chain(
            state
                .error
                .as_ref()
                .filter(|_| !duplicate)
                .map(|error| account_error(error, cx)),
        )
        .chain(state.send_failed.then(|| {
            div()
                .text_sm()
                .text_color(cx.theme().danger)
                .child(strings.get(Text::AccountSendFailed))
        }));
    match layout {
        ProviderLayout::Onboarding(below) => {
            below.extend(details);
            provider_row(header, status, actions)
        }
        ProviderLayout::Settings(sign_out) => div()
            .flex()
            .flex_col()
            .gap_6()
            .min_w_0()
            .child(header)
            .child(status)
            .children(details)
            .child(actions)
            .children(
                matches!(state.view, Some(AccountView::SignedIn { .. })).then(|| {
                    div()
                        .flex()
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .pt_4()
                        .child(sign_out)
                }),
            ),
    }
}

/// The section's buttons, each sending its operation through `route`, and Manage usage.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "Settings' debug fixture flag, passed through"
)]
fn account_actions(
    state: &Accounts,
    route: impl Fn(AccountOp, &mut App) + Clone + 'static,
    fixture: bool,
    cx: &App,
) -> Div {
    let strings = cx.global::<Strings>();
    let button = |id: &'static str, label: Text, op: AccountOp| {
        let route = route.clone();
        Button::new(id)
            .debug_selector(move || id.into())
            .label(strings.get(label))
            .disabled(!state.allows(op))
            .on_click(move |_, _, cx| route(op, cx))
    };
    let mut actions = div()
        .debug_selector(|| "account-actions".into())
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2();
    match &state.view {
        Some(AccountView::SignedOut { .. }) if !state.unsupported => {
            actions = actions.child(button(
                "account-sign-in",
                Text::AccountContinue,
                AccountOp::SignIn,
            ));
        }
        Some(AccountView::SigningIn) if state.unsupported => {
            actions = actions.child(button(
                "account-cancel",
                Text::AccountCancel,
                AccountOp::CancelSignIn,
            ));
        }
        Some(AccountView::SigningIn) => {
            actions = actions
                .child(button(
                    "account-cancel",
                    Text::AccountCancel,
                    AccountOp::CancelSignIn,
                ))
                .child(button(
                    "account-open-again",
                    Text::AccountOpenAgain,
                    AccountOp::SignIn,
                ));
        }
        Some(AccountView::SignedIn { .. }) if state.sign_in_again() => {
            actions = actions
                .debug_selector(|| "account-sign-in-recovery".into())
                .child(button(
                    "account-sign-in-again",
                    Text::AccountSignInAgain,
                    AccountOp::SignIn,
                ));
        }
        Some(AccountView::SignedIn {
            plan: PlanState::Declined,
            recovery: None,
            ..
        }) if !state.unsupported => {
            actions = actions.child(button(
                "account-enable",
                Text::AccountEnable,
                AccountOp::EnablePlan,
            ));
        }
        _ => {}
    }
    if state.allows(AccountOp::Retry) || state.waiting == Some(AccountOp::Retry) {
        actions = actions.child(button(
            "account-retry",
            if matches!(state.view, Some(AccountView::Busy)) {
                Text::AccountTryAgain
            } else {
                Text::Retry
            },
            AccountOp::Retry,
        ));
    }
    if !state.unsupported
        && matches!(
            state.view,
            Some(AccountView::SignedIn {
                plan: PlanState::Enabled | PlanState::Unavailable,
                recovery: None,
                ..
            })
        )
    {
        actions = actions.child(
            Link::new("account-usage")
                .href(MANAGE_USAGE_URL)
                .disabled(state.waiting.is_some())
                .open_with(move |url, _, _, cx| {
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

fn account_header(state: &Accounts, cx: &App) -> Div {
    let strings = cx.global::<Strings>();
    let identity = match &state.view {
        Some(AccountView::SignedIn { email, .. }) => Some(email.as_ref().map_or_else(
            || strings.get(Text::AccountIdentity),
            |email| email.as_str().to_owned(),
        )),
        _ => None,
    };
    div()
        .debug_selector(|| "account-provider".into())
        .min_w_0()
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
                .min_w_0()
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

fn account_error(error: &kumochi_protocol::message::UserError, cx: &App) -> Div {
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
        let row = div().text_sm().text_color(cx.theme().danger);
        #[cfg(test)]
        let row = row.debug_selector(|| "chatgpt-account-error".into());
        row.child(strings.error(error))
    }
}

fn account_notice(text: Text, cx: &App) -> Div {
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
            } else if !state.unsupported {
                lines.push(Text::AccountUsePlan);
            }
            Text::AccountSignedOut
        }
        Some(AccountView::SigningIn) => {
            if !state.unsupported {
                lines.push(Text::AccountReplaceHelp);
            }
            Text::AccountSigningIn
        }
        Some(AccountView::Busy) => {
            if !state.unsupported {
                lines.push(Text::AccountBusyHelp);
            }
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
                if !state.unsupported && state.error.is_none() {
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
    if !state.unsupported {
        (status, lines)
    } else if matches!(state.view, Some(AccountView::SigningOut { .. })) {
        lines.push(Text::UnsupportedPlatform);
        (status, lines)
    } else {
        (Text::UnsupportedPlatform, lines)
    }
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
    use super::{
        AccountRecovery, AccountView, Accounts, Cleanup, PlanState, Text, account_status,
        cleanup_notice,
    };

    #[test]
    fn account_status_prefers_recovery_to_each_plan_state() {
        for plan in [
            PlanState::Enabled,
            PlanState::Declined,
            PlanState::Unavailable,
        ] {
            for (recovery, expected) in [
                (
                    AccountRecovery::SignInAgain,
                    (Text::AccountSessionEnded, vec![]),
                ),
                (
                    AccountRecovery::Retry,
                    (Text::AccountAttention, vec![Text::AccountRetryHelp]),
                ),
            ] {
                let mut state = Accounts::default();
                assert!(state.receive(
                    Some(7),
                    7,
                    AccountView::SignedIn {
                        email: None,
                        plan,
                        recovery: Some(recovery),
                    },
                ));
                assert_eq!(account_status(&state), expected, "{plan:?}, {recovery:?}");
            }
        }
    }

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
        for (revocation_unconfirmed, line) in [
            (false, Text::AccountUsePlan),
            (true, Text::AccountRevocationUnconfirmed),
        ] {
            let view = AccountView::SignedOut {
                revocation_unconfirmed,
            };
            let mut state = Accounts::default();
            assert!(state.receive(Some(7), 7, view));
            assert_eq!(account_status(&state), (Text::AccountSignedOut, vec![line]));
        }
    }
}
