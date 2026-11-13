//! Claude Code's Accounts section: snapshots and explicit connection commands.

use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _};
use gpui_kit::{
    App, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _,
    div,
};
use kumochi_protocol::message::{ClaudeCodeView, ConnectionOp};

use super::SettingsView;
use crate::settings::Settings;
use crate::strings::{Strings, Text};

impl SettingsView {
    fn confirm_claude_disconnect(
        &mut self,
        window: &mut gpui_kit::Window,
        cx: &mut Context<'_, Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        let state = &self.model.read(cx).claude;
        if !state.allows(ConnectionOp::DisconnectClaudeCode) {
            return;
        }
        let generation = state.generation;
        let model = self.model.clone();
        let owner = self.owner.clone();
        let strings = cx.global::<Strings>();
        let title = strings.get(Text::ClaudeDisconnect).into();
        let body = strings.get(Text::ClaudeDisconnectBody).into();
        let cancel = strings.get(Text::AccountDialogCancel).into();
        let confirm = strings.get(Text::ClaudeDisconnect).into();
        let _focus =
            crate::confirmation::open(window, cx, title, body, cancel, confirm, move |_, cx| {
                if model.read(cx).claude.generation == generation {
                    let _gone = owner.update(cx, |owner, cx| {
                        owner.claude_op(ConnectionOp::DisconnectClaudeCode, cx);
                    });
                }
            });
    }

    pub(super) fn claude(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let owner = self.owner.clone();
        let disconnect = Button::new("claude-disconnect")
            .outline()
            .label(cx.global::<Strings>().get(Text::ClaudeDisconnect))
            .disabled(
                !self
                    .model
                    .read(cx)
                    .claude
                    .allows(ConnectionOp::DisconnectClaudeCode),
            )
            .on_click(cx.listener(|view, _, window, cx| {
                view.confirm_claude_disconnect(window, cx);
            }));
        claude_section(
            &self.model,
            move |op, cx| {
                let _gone = owner.update(cx, |owner, cx| owner.settings_claude_op(op, cx));
            },
            Some(disconnect),
            cx,
        )
    }
}

/// The Claude Code section, for every view that shows the connection. `route` sends a button's
/// operation; `disconnect` shows while connected.
pub(in crate::presence) fn claude_section(
    model: &Entity<Settings>,
    route: impl Fn(ConnectionOp, &mut App) + Clone + 'static,
    disconnect: Option<Button>,
    cx: &App,
) -> impl IntoElement {
    let strings = cx.global::<Strings>();
    let state = &model.read(cx).claude;
    let button = |id: &'static str, text: Text, op: ConnectionOp| {
        let route = route.clone();
        Button::new(id)
            .debug_selector(move || id.into())
            .label(strings.get(text))
            .disabled(!state.allows(op))
            .on_click(move |_, _, cx| route(op, cx))
    };
    let mut actions = div().flex().flex_wrap().gap_2();
    if matches!(state.view, Some(ClaudeCodeView::SignedOut { .. })) && !state.connect_screen {
        let model = model.clone();
        actions = actions.child(
            Button::new("claude-review")
                .debug_selector(|| "claude-review".into())
                .label(strings.get(Text::ClaudeReview))
                .disabled(!state.allows(ConnectionOp::FindClaudeCode))
                .on_click(move |_, _, cx| {
                    model.update(cx, |state, cx| {
                        state.claude.open_connect();
                        cx.notify();
                    });
                }),
        );
    }
    if state.connect_screen && matches!(state.view, Some(ClaudeCodeView::SignedOut { .. })) {
        actions = actions.child(button(
            "claude-connect",
            Text::ClaudeConnect,
            ConnectionOp::ConnectClaudeCode,
        ));
    }
    if matches!(state.view, Some(ClaudeCodeView::SigningIn)) {
        actions = actions.child(button(
            "claude-cancel",
            Text::AccountCancel,
            ConnectionOp::CancelClaudeSignIn,
        ));
    }
    if state.allows(ConnectionOp::FindClaudeCode) {
        actions = actions.child(button(
            "claude-find",
            Text::ClaudeFind,
            ConnectionOp::FindClaudeCode,
        ));
    }
    if matches!(state.view, Some(ClaudeCodeView::Ready { .. })) {
        actions = actions.children(disconnect);
    }

    div()
        .id("claude-accounts")
        .flex()
        .flex_col()
        .gap_3()
        .min_w_0()
        .border_t_1()
        .border_color(cx.theme().border)
        .pt_4()
        .child(div().text_lg().child(strings.get(Text::ClaudeProvider)))
        .child(div().text_sm().child(strings.claude_status(state.view)))
        .children(
            matches!(
                state.view,
                Some(ClaudeCodeView::Ready {
                    analytics_on: true,
                    ..
                })
            )
            .then(|| div().text_sm().child(strings.get(Text::ClaudeAnalytics))),
        )
        .children(connect_texts(state).map(|texts| {
            div()
                .flex()
                .flex_col()
                .gap_3()
                .text_sm()
                .children(texts.map(|text| div().child(strings.get(text))))
        }))
        .children(state.send_failed.then(|| {
            div()
                .text_sm()
                .text_color(cx.theme().danger)
                .child(strings.get(Text::AccountSendFailed))
        }))
        .child(actions)
}

fn connect_texts(state: &crate::claude_accounts::ClaudeAccounts) -> Option<[Text; 4]> {
    let signing = state.view == Some(ClaudeCodeView::SigningIn);
    (state.connect_screen || signing).then_some([
        Text::ClaudePolicyNotice,
        if signing {
            Text::ClaudeDuring
        } else {
            Text::ClaudeBefore
        },
        Text::ClaudeFallback,
        Text::ClaudeDeadline,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_accounts::ClaudeAccounts;
    use kumochi_protocol::message::ClaudeVersion;

    #[test]
    fn connect_guidance_precedes_the_command_and_stays_during_sign_in() {
        let mut state = ClaudeAccounts::default();
        state.receive(
            Some(1),
            1,
            ClaudeCodeView::SignedOut {
                version: ClaudeVersion {
                    major: 2,
                    minor: 1,
                    patch: 306,
                },
            },
        );
        assert_eq!(connect_texts(&state), None);
        state.open_connect();
        assert_eq!(
            connect_texts(&state),
            Some([
                Text::ClaudePolicyNotice,
                Text::ClaudeBefore,
                Text::ClaudeFallback,
                Text::ClaudeDeadline,
            ])
        );
        assert!(state.allows(ConnectionOp::ConnectClaudeCode));
        state.receive(Some(1), 1, ClaudeCodeView::SigningIn);
        assert_eq!(
            connect_texts(&state),
            Some([
                Text::ClaudePolicyNotice,
                Text::ClaudeDuring,
                Text::ClaudeFallback,
                Text::ClaudeDeadline,
            ])
        );
        state.receive(Some(1), 1, ClaudeCodeView::Finding);
        assert_eq!(connect_texts(&state), None);
    }
}
