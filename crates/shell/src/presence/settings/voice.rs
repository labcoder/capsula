#[cfg(test)]
use gpui_kit::InteractiveElement as _;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{Context, IntoElement, ParentElement as _, Styled as _, div};
use kumochi_protocol::message::SttUnload;

use super::SettingsView;
use crate::settings::SpeechInputAction;
use crate::strings::{Strings, Text};

const POLICIES: [(SttUnload, Text, &str); 3] = [
    (SttUnload::Kept, Text::VoiceKept, "speech-idle-kept"),
    (
        SttUnload::After15Min,
        Text::Voice15Minutes,
        "speech-idle-15m",
    ),
    (SttUnload::After1Hour, Text::Voice1Hour, "speech-idle-1h"),
];

impl SettingsView {
    pub(super) fn voice(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let state = self.model.read(cx);
        let strings = cx.global::<Strings>();
        let selected = POLICIES
            .iter()
            .position(|(policy, _, _)| Some(*policy) == state.stt_unload_choice());
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_sm().child(strings.get(Text::SpeechInput)))
            .child(
                div()
                    .text_sm()
                    .child(strings.get(state.speech_input_status())),
            )
            .children(
                state
                    .view()
                    .is_none()
                    .then(|| div().text_sm().child(strings.get(Text::SettingsOffline))),
            )
            .child(self.speech_input_model(cx))
            .child(div().text_sm().child(strings.get(Text::VoiceIdlePolicy)))
            .child(
                RadioGroup::vertical("speech-idle-policy")
                    .selected_index(selected)
                    .disabled(state.view().is_none() || state.stt_unload_pending())
                    .children(POLICIES.map(|(_, text, id)| {
                        Radio::new(id).label(strings.get(text)).map(|radio| {
                            #[cfg(test)]
                            let radio = radio.debug_selector(move || id.into());
                            radio
                        })
                    }))
                    .on_change(cx.listener(|view, index, _, cx| {
                        let Some((policy, _, _)) = POLICIES.get(*index) else {
                            return;
                        };
                        let policy = *policy;
                        if view
                            .owner
                            .update(cx, |owner, cx| owner.save_stt_unload(policy, cx))
                            .is_err()
                        {
                            view.model.update(cx, |state, cx| {
                                state.send_stt_unload(policy, |_| false);
                                cx.notify();
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

    fn speech_input_model(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let state = self.model.read(cx);
        let strings = cx.global::<Strings>();
        let intent = state.speech_input_intent();
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .items_start()
            .gap_2()
            .child(div().text_sm().child(strings.get(Text::VoiceDefaultModel)))
            .children(intent.map(|intent| {
                Button::new("speech-model-action")
                    .label(strings.get(intent.action.label()))
                    .disabled(state.speech_input_pending())
                    .when(intent.action == SpeechInputAction::Cancel, |button| {
                        button.accessibility_label(strings.get(Text::VoiceCancelAccessible))
                    })
                    .on_click(cx.listener(move |view, _, window, cx| {
                        if view
                            .owner
                            .update(cx, |owner, cx| owner.speech_input_action(intent, cx))
                            .is_err()
                        {
                            view.model.update(cx, |state, cx| {
                                state.send_speech_input(intent, None, |_| false);
                                cx.notify();
                            });
                        }
                        window.focus(&view.focus, cx);
                    }))
                    .map(|button| {
                        #[cfg(test)]
                        let button = button.debug_selector(|| "speech-model-action".into());
                        button
                    })
            }))
            .children(
                intent
                    .filter(|intent| intent.action == SpeechInputAction::Cancel)
                    .map(|_| {
                        div()
                            .w_full()
                            .text_xs()
                            .child(strings.get(Text::VoiceCancelHelp))
                            .map(|element| {
                                #[cfg(test)]
                                let element =
                                    element.debug_selector(|| "speech-model-cancel-help".into());
                                element
                            })
                    }),
            )
            .children(state.speech_input_pending().then(|| {
                div()
                    .text_xs()
                    .child(strings.get(Text::VoiceRequestPending))
                    .map(|element| {
                        #[cfg(test)]
                        let element = element.debug_selector(|| "speech-model-pending".into());
                        element
                    })
            }))
            .children(state.speech_input_error().map(|error| {
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(strings.error(error))
                    .map(|element| {
                        #[cfg(test)]
                        let element = element.debug_selector(|| "speech-model-refused".into());
                        element
                    })
            }))
            .children(state.speech_input_failure().map(|error| {
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(strings.error(&error))
            }))
            .children(state.speech_input_send_failed().then(|| {
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(strings.get(Text::SettingsUnavailable))
                    .map(|element| {
                        #[cfg(test)]
                        let element = element.debug_selector(|| "speech-model-send-failed".into());
                        element
                    })
            }))
    }
}

#[cfg(test)]
mod tests;
