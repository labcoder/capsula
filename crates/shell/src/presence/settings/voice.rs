use super::SettingsView;
use crate::settings::{STT_UNLOAD_OPTIONS, Save, VoiceAction, VoiceIntent};
use crate::strings::{Strings, Text};
use gpui_kit::base::InteractiveElementExt as _;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Context, Div, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, accesskit::Role, div,
};
use kumochi_protocol::message::SttUnload;
use kumochi_protocol::voice::VoicePart::{self, SpeechInput, SpokenReplies};

fn stt_unload_id(policy: SttUnload) -> &'static str {
    match policy {
        SttUnload::Kept => "speech-idle-kept",
        SttUnload::After15Min => "speech-idle-15m",
        SttUnload::After1Hour => "speech-idle-1h",
    }
}

fn speech_idle_group(
    rows: impl IntoIterator<Item = (&'static str, Text)>,
    strings: &Strings,
) -> RadioGroup {
    RadioGroup::vertical("speech-idle-policy").children(rows.into_iter().map(|(id, text)| {
        let radio = Radio::new(id).label(strings.get(text));
        #[cfg(test)]
        let radio = radio.debug_selector(move || id.into());
        radio
    }))
}

impl SettingsView {
    pub(super) fn voice(&self, window: &Window, cx: &Context<'_, Self>) -> impl IntoElement {
        let state = self.model.read(cx);
        let strings = cx.global::<Strings>();
        let [hold, _, idle, talk_back, replies] = &self.voice_focus;
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .id("settings-voice-content")
                    .debug_selector(|| "settings-voice-content".into())
                    .size_full()
                    .overflow_y_scroll()
                    .lock_scroll_axis()
                    .track_scroll(&self.voice_scroll)
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_4()
                    .child(
                        div()
                            .flex_none()
                            .track_focus(hold)
                            .child(self.hold_to_talk(cx))
                            .children(state.view().is_none().then(|| {
                                div().text_sm().child(strings.get(Text::SettingsOffline))
                            })),
                    )
                    .child(self.voice_model(SpeechInput, cx).flex_none())
                    .child(self.speech_idle(cx).flex_none().track_focus(idle))
                    .child(
                        div()
                            .flex_none()
                            .track_focus(talk_back)
                            .child(self.talk_back(cx)),
                    )
                    .child(
                        self.voice_model(SpokenReplies, cx)
                            .id("spoken-replies-model")
                            .flex_none()
                            .track_focus(replies)
                            .role(Role::Group)
                            .aria_label(strings.get(Text::SpokenReplies))
                            .border_1()
                            .border_color(if replies.is_focused(window) {
                                cx.theme().ring
                            } else {
                                cx.theme().popover
                            }),
                    ),
            )
            .vertical_scrollbar(&self.voice_scroll)
    }

    fn speech_idle(&self, cx: &Context<'_, Self>) -> Div {
        let state = self.model.read(cx);
        let strings = cx.global::<Strings>();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_sm().child(strings.get(Text::VoiceIdlePolicy)))
            .child(
                speech_idle_group(
                    STT_UNLOAD_OPTIONS
                        .into_iter()
                        .map(|(policy, text)| (stt_unload_id(policy), text)),
                    strings,
                )
                .selected_index(state.stt_unload_index())
                .disabled(state.view().is_none() || state.stt_unload_pending())
                .on_change(cx.listener(|view, index, _, cx| {
                    let Some(&(policy, _)) = STT_UNLOAD_OPTIONS.get(*index) else {
                        return;
                    };
                    if view
                        .owner
                        .update(cx, |owner, cx| {
                            owner.request_save(Save::SttUnload(policy), cx)
                        })
                        .is_err()
                    {
                        view.model.update(cx, |state, cx| {
                            if state.apply_save(Save::SttUnload(policy), None).notify {
                                cx.notify();
                            }
                        });
                    }
                })),
            )
            .children(state.stt_unload_pending().then(|| {
                div()
                    .text_xs()
                    .child(strings.get(Text::SettingsSaving))
                    .map(|element| {
                        #[cfg(test)]
                        let element = element.debug_selector(|| "speech-idle-saving".into());
                        element
                    })
            }))
            .children(state.stt_unload_error().map(|error| {
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(strings.error(error))
                    .map(|element| {
                        #[cfg(test)]
                        let element = element.debug_selector(|| "speech-idle-refused".into());
                        element
                    })
            }))
            .children(state.stt_unload_send_failed().then(|| {
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(strings.get(Text::SettingsUnavailable))
                    .map(|element| {
                        #[cfg(test)]
                        let element = element.debug_selector(|| "speech-idle-send-failed".into());
                        element
                    })
            }))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(strings.get(Text::VoiceIdleHelp)),
            )
    }

    fn talk_back(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let state = self.model.read(cx);
        let strings = cx.global::<Strings>();
        let error = state
            .talk_back_send_error()
            .map(|text| strings.get(text))
            .or_else(|| state.talk_back_error().map(|error| strings.error(error)));
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                Checkbox::new("speak-answers-aloud")
                    .label(strings.get(Text::SpeakAnswersAloud))
                    .checked(state.talk_back_choice() == Some(true))
                    .disabled(state.view().is_none() || state.talk_back_pending())
                    .on_change(cx.listener(|view, on, _, cx| {
                        if view
                            .owner
                            .update(cx, |owner, cx| owner.request_save(Save::TalkBack(*on), cx))
                            .is_err()
                        {
                            view.model.update(cx, |state, cx| {
                                if state.apply_save(Save::TalkBack(*on), None).notify {
                                    cx.notify();
                                }
                            });
                        }
                    }))
                    .map(|checkbox| {
                        #[cfg(test)]
                        let checkbox = checkbox.debug_selector(|| "speak-answers-aloud".into());
                        checkbox
                    }),
            )
            .children(
                state
                    .talk_back_pending()
                    .then(|| div().text_xs().child(strings.get(Text::SettingsSaving))),
            )
            .children(state.talk_back_restart_needed().then(|| {
                div()
                    .text_xs()
                    .child(strings.get(Text::SpokenRepliesRestartToHear))
            }))
            .children(error.map(|error| {
                let error = div().text_sm().text_color(cx.theme().danger).child(error);
                #[cfg(test)]
                let error = error.debug_selector(|| "talk-back-error".into());
                error
            }))
    }

    fn hold_to_talk(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let state = self.model.read(cx);
        let strings = cx.global::<Strings>();
        let owner = self.owner.clone();
        let settings = cx.weak_entity();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                Checkbox::new("hold-to-talk")
                    .label(strings.get(Text::HoldToTalk))
                    .checked(state.hold_to_talk_choice() == Some(true))
                    .disabled(state.view().is_none() || state.hold_to_talk_pending())
                    .on_change(move |on, _, cx| {
                        let Some(settings) = settings.upgrade() else {
                            return;
                        };
                        let sent = owner
                            .update(cx, |owner, cx| owner.save_hold_to_talk(*on, cx))
                            .unwrap_or(false);
                        settings.update(cx, |view, cx| {
                            view.hold_notice = (!sent).then_some(Text::SettingsUnavailable);
                            cx.notify();
                        });
                    })
                    .map(|checkbox| {
                        #[cfg(test)]
                        let checkbox = checkbox.debug_selector(|| "hold-to-talk".into());
                        checkbox
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(strings.get(Text::HoldToTalkHelp)),
            )
            .children(
                state
                    .hold_to_talk_pending()
                    .then(|| div().text_xs().child(strings.get(Text::SettingsSaving))),
            )
            .children(state.hold_to_talk_error().map(|error| {
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

    fn voice_model(&self, part: VoicePart, cx: &Context<'_, Self>) -> Div {
        let state = self.model.read(cx);
        let strings = cx.global::<Strings>();
        let (id, heading, model) = match part {
            SpeechInput => ("speech-model", Text::SpeechInput, Text::VoiceDefaultModel),
            SpokenReplies => (
                "spoken-replies-model",
                Text::SpokenReplies,
                Text::SpokenRepliesModel,
            ),
        };
        let pending = state.voice_setup_pending(part);
        let intent = state.voice_setup_intent(part).filter(|intent| {
            part != SpokenReplies
                || intent.action != VoiceAction::Cancel
                || state.voice_setup_pending(SpeechInput)
                || !state
                    .voice_setup_intent(SpeechInput)
                    .is_some_and(|input| input.action == VoiceAction::Cancel)
        });
        let cancel = intent.is_some_and(|intent| intent.action == VoiceAction::Cancel);
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .items_start()
            .gap_2()
            .child(div().text_sm().child(strings.get(heading)))
            .child(div().text_sm().child(strings.get(state.voice_status(part))))
            .child(div().text_sm().child(strings.get(model)))
            .children((part == SpokenReplies).then(|| {
                div()
                    .w_full()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(strings.get(Text::SpokenPlaybackUnavailable))
            }))
            .children(
                (part == SpokenReplies
                    && intent.is_some_and(|intent| {
                        matches!(intent.action, VoiceAction::Install | VoiceAction::Retry)
                    }))
                .then(|| {
                    div()
                        .w_full()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(strings.get(Text::SpokenRepliesInstallHelp))
                }),
            )
            .children(intent.map(|intent| self.voice_setup_button(part, id, intent, cx)))
            .children(cancel.then(|| {
                let help = div()
                    .w_full()
                    .text_xs()
                    .child(strings.get(Text::VoiceCancelHelp));
                #[cfg(test)]
                let help = help.debug_selector(move || format!("{id}-cancel-help"));
                help
            }))
            .children(pending.then(|| {
                let message = div()
                    .text_xs()
                    .child(strings.get(Text::VoiceRequestPending));
                #[cfg(test)]
                let message = message.debug_selector(move || format!("{id}-pending"));
                message
            }))
            .children(state.voice_setup_error(part).map(|error| {
                let message = div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(strings.error(error));
                #[cfg(test)]
                let message = message.debug_selector(move || format!("{id}-refused"));
                message
            }))
            .children(state.voice_setup_failure(part).map(|error| {
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(strings.error(&error))
            }))
            .children(state.voice_setup_send_failed(part).then(|| {
                let message = div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(strings.get(Text::SettingsUnavailable));
                #[cfg(test)]
                let message = message.debug_selector(move || format!("{id}-send-failed"));
                message
            }))
    }

    fn voice_setup_button(
        &self,
        part: VoicePart,
        id: &'static str,
        intent: VoiceIntent,
        cx: &Context<'_, Self>,
    ) -> Button {
        let strings = cx.global::<Strings>();
        let owner = self.owner.clone();
        let settings = cx.weak_entity();
        let accessible = match (part, intent.action) {
            (_, VoiceAction::Cancel) => Some(Text::VoiceCancelAccessible),
            (SpokenReplies, VoiceAction::Install) => Some(Text::SpokenRepliesInstallAccessible),
            (SpokenReplies, VoiceAction::Retry) => Some(Text::SpokenRepliesRetryAccessible),
            _ => None,
        };
        let button = Button::new(format!("{id}-action"))
            .label(strings.get(intent.label()))
            .disabled(self.model.read(cx).voice_setup_pending(part))
            .when_some(accessible, |button, text| {
                button.accessibility_label(strings.get(text))
            })
            .when(part == SpeechInput, |button| {
                button.track_focus(&self.voice_focus[1])
            })
            .on_click(move |_, window, cx| {
                let Some(settings) = settings.upgrade() else {
                    return;
                };
                let gone = owner
                    .update(cx, |owner, cx| owner.voice_setup_action(intent, cx))
                    .is_err();
                if gone || part == SpeechInput {
                    settings.update(cx, |view, cx| {
                        if gone {
                            view.model.update(cx, |state, cx| {
                                if state.apply_save(Save::Voice(intent), None).notify {
                                    cx.notify();
                                }
                            });
                        }
                        if part == SpeechInput {
                            window.focus(&view.focus, cx);
                        }
                    });
                }
            });
        #[cfg(test)]
        let button = button.debug_selector(move || format!("{id}-action"));
        button
    }
}

#[cfg(test)]
mod tests;
