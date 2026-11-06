//! Claude Code's Accounts section: snapshots and explicit connection commands.

use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _};
use gpui_kit::{
    Context, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _, div,
};
use kumochi_protocol::message::{ClaudeCodeView, ConnectionOp};

use super::SettingsView;
use crate::strings::{Strings, Text};

impl SettingsView {
    fn claude_button(
        &self,
        id: &'static str,
        text: Text,
        op: ConnectionOp,
        cx: &Context<'_, Self>,
    ) -> Button {
        let owner = self.owner.clone();
        Button::new(id)
            .label(cx.global::<Strings>().get(text))
            .disabled(!self.model.read(cx).claude.allows(op))
            .on_click(move |_, _, cx| {
                let _gone = owner.update(cx, |owner, cx| owner.claude_op(op, cx));
            })
    }

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
        let strings = cx.global::<Strings>();
        let model = self.model.read(cx);
        let state = &model.claude;
        let mut actions = div().flex().flex_wrap().gap_2();
        if matches!(state.view, Some(ClaudeCodeView::SignedOut { .. })) && !state.connect_screen {
            actions = actions.child(
                Button::new("claude-review")
                    .label(strings.get(Text::ClaudeReview))
                    .disabled(!state.allows(ConnectionOp::FindClaudeCode))
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.model.update(cx, |state, cx| {
                            state.claude.open_connect();
                            cx.notify();
                        });
                    })),
            );
        }
        if state.connect_screen && matches!(state.view, Some(ClaudeCodeView::SignedOut { .. })) {
            actions = actions.child(self.claude_button(
                "claude-connect",
                Text::ClaudeConnect,
                ConnectionOp::ConnectClaudeCode,
                cx,
            ));
        }
        if matches!(state.view, Some(ClaudeCodeView::SigningIn)) {
            actions = actions.child(self.claude_button(
                "claude-cancel",
                Text::AccountCancel,
                ConnectionOp::CancelClaudeSignIn,
                cx,
            ));
        }
        if state.allows(ConnectionOp::FindClaudeCode) {
            actions = actions.child(self.claude_button(
                "claude-find",
                Text::ClaudeFind,
                ConnectionOp::FindClaudeCode,
                cx,
            ));
        }
        if matches!(
            state.view,
            Some(ClaudeCodeView::Ready { .. } | ClaudeCodeView::CleanupPending)
        ) {
            actions = actions.child(
                Button::new("claude-disconnect")
                    .outline()
                    .label(strings.get(Text::ClaudeDisconnect))
                    .disabled(!state.allows(ConnectionOp::DisconnectClaudeCode))
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.confirm_claude_disconnect(window, cx);
                    })),
            );
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
