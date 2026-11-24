use super::super::*;
use super::connection;
use super::machine::{ReviewControl, review_control};
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::{ScrollHandle, StatefulInteractiveElement as _};
use kumochi_protocol::message::{IssueCard, IssueOp};

pub(in crate::presence::messages) struct Preview {
    title: SharedString,
    details: SharedString,
    consequence: SharedString,
    focus: FocusHandle,
    scroll: ScrollHandle,
}

impl std::fmt::Debug for Preview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuePreview")
            .field("title_len", &self.title.len())
            .field("details_len", &self.details.len())
            .finish_non_exhaustive()
    }
}

impl Preview {
    pub(in crate::presence::messages) fn new(card: &IssueCard, cx: &App) -> Self {
        let strings = cx.global::<Strings>();
        Self {
            title: strings.issue_title(card.title.as_str()).into(),
            details: strings.issue_details(card).into(),
            consequence: strings.get(Text::IssueConsentConsequence).into(),
            focus: cx.focus_handle().tab_stop(true),
            scroll: ScrollHandle::new(),
        }
    }

    pub(in crate::presence::messages) fn texts(&self) -> [gpui_kit::Text; 3] {
        [
            gpui_kit::Text::new("issue-preview-title".into(), self.title.clone()),
            gpui_kit::Text::new("issue-preview-details".into(), self.details.clone()),
            gpui_kit::Text::new("issue-preview-consequence".into(), self.consequence.clone()),
        ]
    }
}

impl Render for Preview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .relative()
            .h(window.rem_size() * 18.0)
            .min_h_0()
            .w_full()
            .when(self.focus.is_focused(window), |this| {
                this.focus_ring_style(window, cx)
            })
            .child(
                div()
                    .id("issue-preview-scroll")
                    .debug_selector(|| "issue-preview-scroll".into())
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .track_focus(&self.focus)
                    .key_context("IssuePreview")
                    .on_key_down(cx.listener(|view, event: &gpui_kit::KeyDownEvent, _, cx| {
                        let mut offset = view.scroll.offset();
                        let page = view.scroll.bounds().size.height * 0.9;
                        match event.keystroke.key.as_str() {
                            "pageup" => offset.y += page,
                            "pagedown" => offset.y -= page,
                            "up" => offset.y += px(24.0),
                            "down" => offset.y -= px(24.0),
                            "home" => offset.y = px(0.0),
                            "end" => offset.y = -view.scroll.max_offset().y,
                            _ => return,
                        }
                        offset.y = offset.y.clamp(-view.scroll.max_offset().y, px(0.0));
                        view.scroll.set_offset(offset);
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .child(
                        div()
                            .debug_selector(|| "issue-preview-fields".into())
                            .flex()
                            .flex_col()
                            .gap_3()
                            .text_sm()
                            .children(self.texts()),
                    ),
            )
            .child(Scrollbar::vertical(&self.scroll))
    }
}

#[derive(Clone)]
pub(in crate::presence::messages) struct Labels {
    open: SharedString,
    check: SharedString,
    cancel: SharedString,
    dismiss: SharedString,
    replacement: SharedString,
}

impl Labels {
    pub(in crate::presence::messages) fn new(strings: &Strings) -> Self {
        Self {
            open: strings.get(Text::IssueConsentOpen).into(),
            check: strings.get(Text::IssueCheckAgain).into(),
            cancel: strings.get(Text::IssueConsentCancel).into(),
            dismiss: strings.get(Text::IssueDismiss).into(),
            replacement: strings.get(Text::IssueReplacement).into(),
        }
    }

    fn unavailable(&self, listing_hint: Option<Text>, cx: &App) -> impl IntoElement {
        let (id, text) = if let Some(hint) = listing_hint {
            (
                "issue-repository-listing",
                cx.global::<Strings>().get(hint).into(),
            )
        } else {
            ("issue-replacement", self.replacement.clone())
        };
        div()
            .debug_selector(move || id.into())
            .child(gpui_kit::Text::new(id.into(), text))
    }
}

pub(in crate::presence::messages) fn controls(
    draft: DraftId,
    model: &Threads,
    owner: &WeakEntity<Presence>,
    view: &WeakEntity<Messages>,
    labels: &Labels,
    cx: &App,
) -> gpui_kit::AnyElement {
    let Some(entry) = model.issue(draft) else {
        return div().into_any_element();
    };
    let Some(card) = entry.card().filter(|card| {
        model.loaded
            && model.open == Some(card.thread)
            && !model.deleting.contains(&card.thread)
            && !entry.loading()
    }) else {
        return div().into_any_element();
    };
    let actionable = matches!(
        card.state,
        ActionState::Draft | ActionState::Failed(_) | ActionState::Unknown { .. }
    );
    let mut controls = div().flex().flex_col().gap_2();
    if actionable {
        let review = review_control(&entry, connection(owner, cx));
        let pending = entry.pending();
        let mut buttons = div().flex().gap_2();
        if let ReviewControl::Open { disabled } = review {
            let view = view.clone();
            buttons = buttons.child(
                Button::new(("issue-file", draft.0))
                    .debug_selector(move || format!("issue-file-{}", draft.0))
                    .label(labels.open.clone())
                    .disabled(disabled)
                    .on_click(move |_, window, cx| {
                        let _gone = view.update(cx, |view, cx| {
                            view.open_issue_confirmation(draft, window, cx)
                        });
                    }),
            );
        }
        if matches!(card.state, ActionState::Unknown { .. }) {
            let view = view.clone();
            buttons = buttons.child(
                Button::new(("issue-check", draft.0))
                    .debug_selector(move || format!("issue-check-{}", draft.0))
                    .label(labels.check.clone())
                    .outline()
                    .disabled(pending)
                    .on_click(move |_, window, cx| {
                        let _gone = view.update(cx, |view, cx| {
                            view.issue_action(IssueOp::CheckAgain { draft }, window, cx);
                        });
                    }),
            );
        }
        let view = view.clone();
        if let Some(review) = entry.pending_review_request() {
            let owner = owner.clone();
            buttons = buttons.child(
                Button::new(("issue-cancel-review", draft.0))
                    .debug_selector(move || format!("issue-cancel-review-{}", draft.0))
                    .label(labels.cancel.clone())
                    .outline()
                    .on_click(move |_, window, cx| {
                        if window.is_window_active() && window.is_visible() {
                            let _gone =
                                owner.update(cx, |owner, cx| owner.cancel_issue_review(review, cx));
                        }
                    }),
            );
        }
        buttons = buttons.child(
            Button::new(("issue-dismiss", draft.0))
                .debug_selector(move || format!("issue-dismiss-{}", draft.0))
                .label(labels.dismiss.clone())
                .ghost()
                .disabled(pending)
                .on_click(move |_, window, cx| {
                    let _gone = view.update(cx, |view, cx| {
                        view.issue_action(IssueOp::Discard { draft }, window, cx);
                    });
                }),
        );
        controls = controls.child(buttons);
        if let ReviewControl::Unavailable { listing_hint } = review {
            controls = controls.child(labels.unavailable(listing_hint, cx));
        }
    }
    controls.into_any_element()
}
