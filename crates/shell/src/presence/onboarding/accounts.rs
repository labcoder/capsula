//! The account step: each connection's section as Settings draws it, and the sign-ins this
//! window asked for (`presence/sign_ins.rs` moves their marks). A dismissal (Finish later, Esc,
//! the window's close) cancels one that hasn't ended; Continue, Not now and the finish hand a
//! waiting one over to the rest of the app.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::{
    App, Context, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _, div,
};
use kumochi_protocol::message::{AccountOp, AccountView, ClaudeCodeView, ConnectionOp};

use super::OnboardingView;
use crate::onboarding::{Step, account_complete};
use crate::presence::Presence;
use crate::presence::settings::accounts::account_section;
use crate::presence::settings::claude::claude_section;
use crate::settings::{Field, Settings};
use crate::sign_in_mark::Shows;
use crate::strings::Strings;

/// A model connection is usable: the step's button reads Continue instead of Not now.
pub(super) fn usable(state: &Settings) -> bool {
    account_complete(state.accounts.view.as_ref())
        || matches!(state.claude.view, Some(ClaudeCodeView::Ready { .. }))
}

impl OnboardingView {
    /// A heading, then each connection's section without Sign out or Disconnect. Their buttons
    /// are this window's, so a sign-in they start is its own.
    pub(super) fn accounts(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let model = self.model.read(cx);
        let [heading, help] = strings.onboarding_accounts(model.value(Field::Assistant));
        let (chatgpt, claude) = (self.owner.clone(), self.owner.clone());
        div()
            .id("onboarding-accounts")
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_xl().child(heading))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(help),
                            ),
                    )
                    .child(account_section(
                        model,
                        move |op, cx| {
                            let _gone =
                                chatgpt.update(cx, |owner, cx| owner.onboarding_account_op(op, cx));
                        },
                        false,
                        None,
                        cx,
                    ))
                    .child(claude_section(
                        &self.model,
                        move |op, cx| {
                            let _gone =
                                claude.update(cx, |owner, cx| owner.onboarding_claude_op(op, cx));
                        },
                        None,
                        cx,
                    )),
            )
    }
}

impl Presence {
    /// A `ChatGPT` button in the account step. A sign-in it queued is this window's, shown when
    /// Open again replaced a wait already showing.
    pub(super) fn onboarding_account_op(&mut self, op: AccountOp, cx: &mut Context<'_, Self>) {
        let generation = self.session.as_ref().map(crate::link::Session::generation);
        let shown = matches!(
            self.settings.read(cx).accounts.view,
            Some(AccountView::SigningIn)
        );
        if self.account_op(op, cx)
            && matches!(op, AccountOp::SignIn | AccountOp::EnablePlan)
            && let Some(generation) = generation
        {
            self.onboarding_chatgpt.sent(generation);
            if shown {
                self.onboarding_chatgpt.view(generation, Shows::Waiting);
            }
        }
    }

    /// A Claude Code button in the account step. A connect it queued is this window's.
    pub(super) fn onboarding_claude_op(&mut self, op: ConnectionOp, cx: &mut Context<'_, Self>) {
        let generation = self.session.as_ref().map(crate::link::Session::generation);
        if self.claude_op(op, cx)
            && op == ConnectionOp::ConnectClaudeCode
            && let Some(generation) = generation
        {
            self.onboarding_claude.sent(generation);
        }
    }

    /// A `ChatGPT` sign-in this window asked for completed: the plan line shows.
    pub(in crate::presence) fn onboarding_signed_in(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(panel) = &self.onboarding {
            panel.view.update(cx, |view, cx| {
                view.plan = true;
                cx.notify();
            });
        }
    }

    /// The window was dismissed: each sign-in it asked for that hasn't ended is cancelled once,
    /// past the model's gate, since the brain may not have answered or shown it yet. One that
    /// can't be sent stays owed; the window closes either way.
    pub(super) fn cancel_onboarding_sign_ins(&mut self, cx: &mut Context<'_, Self>) {
        if self.onboarding_chatgpt.take() && self.send_chatgpt_cancel(cx) {
            self.onboarding_chatgpt.cancel_sent();
        }
        if self.onboarding_claude.take() && self.send_claude_cancel(cx) {
            self.onboarding_claude.cancel_sent();
        }
    }

    /// Whether the window owns sign-ins: only until it leaves the account step, which hands its
    /// own over, so after that it adopts nothing for the rest of its life.
    pub(in crate::presence) fn onboarding_owns_sign_ins(&self, cx: &App) -> bool {
        self.onboarding
            .as_ref()
            .is_some_and(|panel| panel.view.read(cx).state.step() != Step::Shortcut)
    }

    /// The window opened over a waiting sign-in: after a shell restart, it's this window's.
    pub(super) fn adopt_onboarding_sign_ins(&mut self, cx: &Context<'_, Self>) {
        if !self.onboarding_owns_sign_ins(cx) {
            return;
        }
        let state = self.settings.read(cx);
        if let (Some(AccountView::SigningIn), Some(generation)) =
            (&state.accounts.view, state.accounts.generation)
        {
            self.onboarding_chatgpt.adopt(generation);
        }
        if let (Some(ClaudeCodeView::SigningIn), Some(generation)) =
            (state.claude.view, state.claude.generation)
        {
            self.onboarding_claude.adopt(generation);
        }
    }
}
