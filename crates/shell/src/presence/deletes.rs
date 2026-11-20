//! Deleting threads from the UI thread: the confirmed delete takes a slot and is sent, resent
//! after each connection's `Scenes` and retried while the control queue is full; its answer
//! fences the thread's runs, then drops what the panels show of it.

use std::time::Duration;

use gpui_kit::Context;
use kumochi_protocol::message::{Deleted, RunId, ThreadId};

use super::Presence;

/// How soon a delete the control queue had no room for is tried again [guess].
const DELETE_RETRY: Duration = Duration::from_millis(250);

impl Presence {
    /// The user confirmed deleting `thread`. Nothing happens when every slot is held.
    pub(super) fn delete_thread(&mut self, thread: ThreadId, cx: &mut Context<'_, Self>) {
        self.cancel_voice(cx);
        if !self.runs.confirm_delete(thread) {
            return;
        }
        self.refresh_conversation(cx);
        self.link.delete(thread);
        for model in [&self.threads, &self.chat] {
            model.update(cx, |state, cx| {
                state.delete_failed = false;
                cx.notify();
            });
        }
        self.send_deletes(cx);
    }

    /// Sends the deletes this connection hasn't had, once its `Scenes` is applied. While the
    /// control queue has no room, a timer tries again.
    pub(super) fn send_deletes(&mut self, cx: &mut Context<'_, Self>) {
        self.sync_deletes(cx);
        let Some(session) = self
            .session
            .as_ref()
            .filter(|_| self.runs.described() || self.presenter.membership_ready())
        else {
            return;
        };
        if self.link.send_deletes(session) || self.delete_retry.is_some() {
            return;
        }
        self.delete_retry = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DELETE_RETRY).await;
            let _gone = this.update(cx, |this, cx| {
                this.delete_retry = None;
                this.send_deletes(cx);
            });
        }));
    }

    /// The brain answered `thread`'s delete: the runs fence first, then its rows and messages
    /// go. Its drawing goes on the data pipe's `ScenesRemoved`.
    pub(super) fn thread_deleted(
        &mut self,
        thread: ThreadId,
        outcome: Deleted,
        last_run: Option<RunId>,
        cx: &mut Context<'_, Self>,
    ) {
        let held = self.runs.deleting().contains(&thread);
        let credit = self.runs.deleted(thread, outcome, last_run);
        if held && outcome != Deleted::NotHidden {
            self.voice_thread_deleted(thread);
        }
        self.grant(credit);
        self.link.settle_delete(thread);
        if outcome != Deleted::NotHidden {
            self.presenter.discard_replacement();
            self.draw_changed(cx);
        }
        if held {
            for model in [&self.threads, &self.chat] {
                model.update(cx, |state, cx| {
                    if outcome == Deleted::NotHidden {
                        state.delete_failed = true;
                        // The brain sends no events marker for a deleting thread; it stays, so
                        // its open page is read again.
                        state.events_changed(thread);
                    } else {
                        state.deleted(thread);
                    }
                    cx.notify();
                });
            }
        }
        self.sync_deletes(cx);
        self.request_pages(cx);
    }

    /// Shows which threads are deleting and whether a slot is free.
    pub(super) fn sync_deletes(&self, cx: &mut Context<'_, Self>) {
        let deleting = self.runs.deleting().to_vec();
        let full = !self.runs.can_delete();
        for model in [&self.threads, &self.chat] {
            model.update(cx, |state, cx| {
                if state.deleting != deleting || state.deletes_full != full {
                    state.deleting.clone_from(&deleting);
                    if state.open.is_some_and(|thread| deleting.contains(&thread)) {
                        state.retire_issue_confirmation();
                    }
                    state.deletes_full = full;
                    cx.notify();
                }
            });
        }
    }
}
