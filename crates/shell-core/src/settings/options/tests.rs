use kumochi_protocol::message::{
    AssistantName,
    ConnectionKind::{AnthropicKey, ChatGpt, ClaudeCode},
    DockPosition, Effort, ErrorKey, MaxFrameRate, ModelChoice, ModelInfo, ModelName, ModelSlug,
    SettingsView, ThreadId, UserError,
};

use super::{Settings, Strings};
use crate::settings::Update;

#[test]
fn a_key_row_always_names_the_key_and_another_only_on_a_shared_name() {
    let row = |slug, name| ModelInfo {
        slug: ModelSlug::new(slug).expect("slug"),
        display_name: ModelName::new(name).expect("name"),
        supported_efforts: None,
    };
    let strings = Strings::english();
    let mut model = Settings::default();
    assert_eq!(model.default_model_options(&strings), (Vec::new(), None));
    model.set_models(ChatGpt, vec![row("m-0", "GPT Luna")]);
    model.set_models(
        AnthropicKey,
        vec![
            row("claude-haiku-5-5", "Claude Haiku 5.5"),
            row("claude-opus-5-5", "Claude Opus 5.5"),
        ],
    );
    model.set_models(
        ClaudeCode,
        vec![
            row("haiku", "Claude Haiku 5.5"),
            row("sonnet", "Claude Sonnet 5.5"),
        ],
    );
    let mut snapshot = saved();
    snapshot.default_model = Some(ModelChoice {
        model: ModelSlug::new("m-0").expect("slug"),
        effort: Effort::Low,
        connection: AnthropicKey,
    });
    model.set_view(snapshot, Update::Unsolicited);
    let (choices, checked) = model.default_model_options(&strings);
    assert_eq!(checked, Some(0));
    assert_eq!(
        choices
            .iter()
            .map(|(connection, slug, label)| (*connection, slug.as_str(), label.as_str()))
            .collect::<Vec<_>>(),
        [
            (AnthropicKey, "m-0", "m-0 (Anthropic API key)"),
            (ChatGpt, "m-0", "GPT Luna"),
            (
                AnthropicKey,
                "claude-haiku-5-5",
                "Claude Haiku 5.5 (Anthropic API key)"
            ),
            (
                AnthropicKey,
                "claude-opus-5-5",
                "Claude Opus 5.5 (Anthropic API key)"
            ),
            (ClaudeCode, "haiku", "Claude Haiku 5.5 (Claude Code)"),
            (ClaudeCode, "sonnet", "Claude Sonnet 5.5"),
        ]
    );
    assert!(model.send_default_model(ClaudeCode, ModelSlug::new("haiku").expect("slug"), |_| true));
    let (queued, checked) = model.default_model_options(&strings);
    assert_eq!(checked, Some(3));
    assert_eq!(queued, choices[1..]);
}

#[test]
fn the_checked_dock_cell_follows_saved_pending_and_refused_choices() {
    for (position, index) in [
        (DockPosition::TopLeft, 0),
        (DockPosition::TopCenter, 1),
        (DockPosition::TopRight, 2),
        (DockPosition::MiddleLeft, 3),
        (DockPosition::Center, 4),
        (DockPosition::MiddleRight, 5),
        (DockPosition::BottomLeft, 6),
        (DockPosition::BottomCenter, 7),
        (DockPosition::BottomRight, 8),
    ] {
        let mut model = Settings::default();
        assert_eq!(model.dock_position_index(), None);
        let mut snapshot = saved();
        snapshot.dock_position = position;
        model.set_view(snapshot, Update::Unsolicited);
        assert_eq!(model.dock_position_index(), Some(index));
        model.send_dock_position(DockPosition::BottomLeft, |_| true);
        assert_eq!(model.dock_position_index(), Some(6));
        model.send_dock_position(DockPosition::Center, |_| true);
        assert_eq!(model.dock_position_index(), Some(4));
        let mut answer = saved();
        answer.dock_position = DockPosition::BottomLeft;
        model.set_view(answer.clone(), Update::Answer);
        assert_eq!(model.dock_position_index(), Some(4));
        model.error(UserError::from(ErrorKey::SettingsRefused));
        assert_eq!(model.dock_position_index(), Some(6));
        model.set_view(answer, Update::Answer);
        assert_eq!(model.dock_position_index(), Some(6));
    }
}

#[test]
fn a_quick_reversal_lands_on_an_unchecked_radio() {
    assert_eq!(
        Settings::frame_rate_options()
            .map(MaxFrameRate::fps)
            .collect::<Vec<_>>(),
        [None, Some(30), Some(60), Some(72), Some(120), Some(144)]
    );
    let mut model = Settings::default();
    assert_eq!(model.set_view(saved(), Update::Unsolicited), None);
    assert_eq!(model.frame_rate_index(), 0);
    let rate = Settings::frame_rate_at(5).expect("a radio");
    assert_eq!(rate.fps(), Some(144));
    assert!(model.send_max_frame_rate(rate, |_| true));
    assert_eq!(
        model.frame_rate_index(),
        5,
        "144 shows checked before the answer"
    );
    let automatic = Settings::frame_rate_at(0).expect("a radio");
    assert_ne!(
        model.frame_rate_index(),
        0,
        "so Automatic is unchecked and takes the click"
    );
    assert!(model.send_max_frame_rate(automatic, |_| true));
    assert_eq!(model.frame_rate_index(), 0);
    assert_eq!(Settings::frame_rate_at(6), None);
}

fn saved() -> SettingsView {
    SettingsView {
        assistant_name: AssistantName::new("Kumo").expect("valid"),
        user_name: None,
        onboarded: true,
        onboarding_from: kumochi_protocol::message::OnboardingStep::Welcome,
        main_thread: ThreadId(1),
        max_frame_rate: MaxFrameRate::AUTOMATIC,
        default_model: None,
        dock_position: DockPosition::TopCenter,
        show_in_taskbar: false,
        talk_back: false,
        stt_unload: kumochi_protocol::message::SttUnload::Kept,
        hold_to_talk: kumochi_protocol::message::DEFAULT_HOLD_TO_TALK,
        screen_holds: kumochi_protocol::message::DEFAULT_SCREEN_HOLDS,
        show_screen: kumochi_protocol::message::DEFAULT_SHOW_SCREEN,
        shortcut: kumochi_protocol::message::ShortcutView::Default,
    }
}
