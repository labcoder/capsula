//! The Privacy tab: whether a hold also sends the model a picture of the screen.

#[cfg(test)]
use gpui_kit::InteractiveElement as _;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{Context, IntoElement, ParentElement as _, Styled as _, div};

use super::SettingsView;
use crate::strings::{Strings, Text};

impl SettingsView {
    pub(super) fn privacy(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let state = self.model.read(cx);
        let strings = cx.global::<Strings>();
        let assistant = state.view().map_or(
            kumochi_protocol::constants::DEFAULT_ASSISTANT_NAME,
            |view| view.assistant_name.as_str(),
        );
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                Checkbox::new("show-screen")
                    .label(strings.show_screen(assistant))
                    .checked(state.show_screen_choice() == Some(true))
                    .disabled(state.view().is_none() || state.show_screen_pending())
                    .on_change(cx.listener(|view, on, _, cx| {
                        let sent = view
                            .owner
                            .update(cx, |owner, cx| owner.save_show_screen(*on, cx))
                            .unwrap_or(false);
                        view.hold_notice = (!sent).then_some(Text::SettingsUnavailable);
                        cx.notify();
                    }))
                    .map(|checkbox| {
                        #[cfg(test)]
                        let checkbox = checkbox.debug_selector(|| "show-screen".into());
                        checkbox
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(strings.show_screen_help()),
            )
            .children(
                state
                    .show_screen_pending()
                    .then(|| div().text_xs().child(strings.get(Text::SettingsSaving))),
            )
            .children(state.show_screen_error().map(|error| {
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(strings.error(error))
            }))
            .children(self.hold_notice.map(|notice| {
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(strings.get(notice))
            }))
    }
}
