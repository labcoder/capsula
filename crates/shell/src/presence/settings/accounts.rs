//! Accounts composes the brain's snapshot; controls only request operations.

use gpui_kit::assets::IconName;
use gpui_kit::base::Link;
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _};
use gpui_kit::{
    AnyElement, App, Context, Div, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Styled as _, Window, div,
};
use kumochi_protocol::message::{AccountOp, AccountView, MANAGE_USAGE_URL, PlanState};

use super::SettingsView;
use crate::accounts::{Accounts, StatusLine};
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
        let forget = match model.accounts.view {
            Some(AccountView::SavedSignInUnavailable {
                forget: Some(revision),
                ..
            }) => Some(AccountOp::ForgetSavedSignIn(revision)),
            _ => None,
        };
        let sign_out = if let Some(op) = forget {
            Button::new("account-forget")
                .debug_selector(|| "account-forget".into())
                .outline()
                .label(cx.global::<Strings>().get(Text::AccountForget))
                .disabled(!model.accounts.allows(op))
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.confirm_forget(op, window, cx);
                }))
        } else {
            Button::new("account-sign-out")
                .debug_selector(|| "account-sign-out".into())
                .outline()
                .label(cx.global::<Strings>().get(Text::AccountSignOut))
                .disabled(!model.accounts.allows(AccountOp::SignOut))
                .on_click(cx.listener(|view, _, window, cx| {
                    view.confirm_sign_out(window, cx);
                }))
        };
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
        let strings = cx.global::<Strings>();
        let body = format!(
            "{}\n\n{}",
            strings.get(Text::AccountSignOutBody),
            strings.get(Text::AccountSignOutRegistration),
        );
        let texts = (Text::AccountSignOutTitle, body, Text::AccountSignOutConfirm);
        self.confirm_account_op(AccountOp::SignOut, texts, window, cx);
    }

    /// Asks before giving up the saved sign-in at the revision its view offered.
    fn confirm_forget(&mut self, op: AccountOp, window: &mut Window, cx: &mut Context<'_, Self>) {
        let body = cx.global::<Strings>().get(Text::AccountForgetBody);
        let texts = (Text::AccountForgetTitle, body, Text::AccountForgetConfirm);
        self.confirm_account_op(op, texts, window, cx);
    }

    /// Sends `op` once confirmed, only if this pair still allows it then.
    fn confirm_account_op(
        &mut self,
        op: AccountOp,
        (title, body, confirm): (Text, String, Text),
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        let state = &self.model.read(cx).accounts;
        if !state.allows(op) {
            return;
        }
        let Some(generation) = state.generation else {
            return;
        };
        let strings = cx.global::<Strings>();
        let view = cx.weak_entity();
        let title = strings.get(title).into();
        let cancel = strings.get(Text::AccountDialogCancel).into();
        let confirm = strings.get(confirm).into();
        let dialog = crate::confirmation::open_during_exit(
            window,
            cx,
            title,
            body.into(),
            cancel,
            confirm,
            move |confirmed, _, cx| {
                let owner = view.update(cx, |view, cx| {
                    if view
                        .confirmation
                        .as_ref()
                        .is_none_or(|(current, _, _)| *current != generation)
                    {
                        return None;
                    }
                    view.confirmation = None;
                    let accounts = &view.model.read(cx).accounts;
                    (confirmed && accounts.generation == Some(generation) && accounts.allows(op))
                        .then(|| view.owner.clone())
                });
                if let Ok(Some(owner)) = owner {
                    let _gone = owner.update(cx, |owner, cx| match owner.session.as_ref() {
                        Some(session) if session.generation() == generation => {
                            owner.account_op(op, cx);
                        }
                        None => owner.settings.update(cx, |state, cx| {
                            state.accounts.send(op, |_| false);
                            cx.notify();
                        }),
                        Some(_) => {}
                    });
                }
            },
        );
        if let Some(dialog) = dialog {
            self.confirmation = Some((generation, op, dialog));
        }
    }
}

/// Whether `view` still shows the button that opened `op`'s confirmation.
pub(super) fn offers(view: Option<&AccountView>, op: AccountOp) -> bool {
    match (view, op) {
        (Some(AccountView::SignedIn { .. }), AccountOp::SignOut) => true,
        (
            Some(AccountView::SavedSignInUnavailable {
                forget: Some(offered),
                ..
            }),
            AccountOp::ForgetSavedSignIn(revision),
        ) => *offered == revision,
        _ => false,
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
    let (status, lines) = state.status(model.view());
    let header = account_header(state, cx);
    let status = div()
        .debug_selector(|| "account-status".into())
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .child(strings.get(status));
    let actions = account_actions(state, route, fixture, cx);
    let details = lines.iter().map(|line| account_notice(line, cx));
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
                matches!(
                    state.view,
                    Some(
                        AccountView::SignedIn { .. }
                            | AccountView::SavedSignInUnavailable {
                                forget: Some(_),
                                ..
                            }
                    )
                )
                .then(|| {
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
        Some(AccountView::SignedOut { .. }) => {
            actions = actions.child(button(
                "account-sign-in",
                Text::AccountContinue,
                AccountOp::SignIn,
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
        }) => {
            actions = actions.child(button(
                "account-enable",
                Text::AccountEnable,
                AccountOp::EnablePlan,
            ));
        }
        _ => {}
    }
    actions = actions.children(account_recovery_button(state, button));
    if matches!(
        state.view,
        Some(AccountView::SignedIn {
            plan: PlanState::Enabled | PlanState::Unavailable,
            recovery: None,
            ..
        })
    ) {
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

fn account_recovery_button(
    state: &Accounts,
    button: impl FnOnce(&'static str, Text, AccountOp) -> Button,
) -> Option<Button> {
    if matches!(
        state.view,
        Some(AccountView::SavedSignInUnavailable {
            checkable: true,
            ..
        })
    ) {
        return Some(
            button(
                "account-check-again",
                Text::AccountCheckAgain,
                AccountOp::CheckSavedSignIn,
            )
            .outline(),
        );
    }
    if state.allows(AccountOp::LoadModels) || state.waiting == Some(AccountOp::LoadModels) {
        return Some(button(
            "account-load-models",
            Text::AccountLoadModels,
            AccountOp::LoadModels,
        ));
    }
    if !state.allows(AccountOp::Retry) && state.waiting != Some(AccountOp::Retry) {
        return None;
    }
    let label = if matches!(state.view, Some(AccountView::Busy)) {
        Text::AccountTryAgain
    } else {
        Text::Retry
    };
    Some(button("account-retry", label, AccountOp::Retry))
}

fn account_header(state: &Accounts, cx: &App) -> Div {
    let strings = cx.global::<Strings>();
    let identity = state.status_identity(strings);
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

fn account_notice(line: &StatusLine<'_>, cx: &App) -> Div {
    let strings = cx.global::<Strings>();
    let notice = || div().flex().flex_col().gap_2().text_sm();
    match *line {
        StatusLine::Backlog { lead, followups } => div()
            .flex()
            .flex_col()
            .gap_3()
            .max_w(gpui_kit::rems(36.))
            .text_sm()
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(cx.theme().danger)
                    .child(strings.get(lead)),
            )
            .children(followups.map(|text| {
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(strings.get(text))
            })),
        StatusLine::Error(error) => {
            let row = div().text_sm().text_color(cx.theme().danger);
            #[cfg(test)]
            let row = row.debug_selector(|| "chatgpt-account-error".into());
            row.child(strings.error(error))
        }
        StatusLine::SendFailed(text) => div()
            .text_sm()
            .text_color(cx.theme().danger)
            .child(strings.get(text)),
        StatusLine::Local {
            heading,
            error,
            help,
        } => notice()
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .child(strings.get(heading)),
            )
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(cx.theme().danger)
                    .child(strings.get(error)),
            )
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(strings.get(help)),
            ),
        StatusLine::Remote { heading, note } => notice()
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .child(strings.get(heading)),
            )
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(strings.get(note)),
            ),
        StatusLine::Note(text) => notice()
            .text_color(cx.theme().muted_foreground)
            .child(strings.get(text)),
    }
}
