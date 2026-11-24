use kumochi_protocol::message::{
    ConnectionKind::{self, AnthropicKey, ChatGpt, ClaudeCode},
    Effort, ModelChoice, ModelInfo, ModelName, ModelSlug, ThreadId,
};

use super::{
    AssistantName, DockPosition, ErrorKey, MaxFrameRate, Settings, SettingsOp, SettingsView,
    Update, UserError,
};
use crate::strings::Strings;

fn info(slug: &str) -> ModelInfo {
    ModelInfo {
        slug: ModelSlug::new(slug).expect("slug"),
        display_name: ModelName::new(slug).expect("name"),
        supported_efforts: None,
    }
}

fn choice(slug: &str, effort: Effort, connection: ConnectionKind) -> ModelChoice {
    ModelChoice {
        model: ModelSlug::new(slug).expect("slug"),
        effort,
        connection,
    }
}

fn saved(default_model: ModelChoice) -> SettingsView {
    SettingsView {
        assistant_name: AssistantName::new("Kumo").expect("valid"),
        user_name: None,
        onboarded: true,
        onboarding_from: kumochi_protocol::message::OnboardingStep::Welcome,
        main_thread: ThreadId(1),
        max_frame_rate: MaxFrameRate::AUTOMATIC,
        default_model: Some(default_model),
        dock_position: DockPosition::TopCenter,
        show_in_taskbar: false,
        talk_back: false,
        stt_unload: kumochi_protocol::message::SttUnload::Kept,
        hold_to_talk: kumochi_protocol::message::DEFAULT_HOLD_TO_TALK,
        screen_holds: kumochi_protocol::message::DEFAULT_SCREEN_HOLDS,
        show_screen: kumochi_protocol::message::DEFAULT_SHOW_SCREEN,
    }
}

/// Every connection's catalog is offered and an empty one takes it off offer; a pick keeps its
/// connection's effort, starts another at Low, and a refused one returns to the saved default.
#[test]
fn the_picker_offers_served_catalogs_and_a_refusal_returns_the_saved_default() {
    let luna = choice("m-0", Effort::High, ConnectionKind::ChatGpt);
    let mut state = Settings::default();
    state.set_view(saved(luna.clone()), Update::Unsolicited);
    state.set_models(ConnectionKind::ChatGpt, vec![info("m-0"), info("m-1")]);
    state.set_models(ConnectionKind::AnthropicKey, vec![info("k-0")]);
    state.set_models(ConnectionKind::ClaudeCode, vec![info("c-0")]);
    let offered: Vec<_> = state
        .model_choices()
        .map(|(connection, model)| (connection, model.slug.as_str().to_owned()))
        .collect();
    assert_eq!(
        offered,
        [
            (ConnectionKind::ChatGpt, "m-0".to_owned()),
            (ConnectionKind::ChatGpt, "m-1".to_owned()),
            (ConnectionKind::AnthropicKey, "k-0".to_owned()),
            (ConnectionKind::ClaudeCode, "c-0".to_owned()),
        ]
    );
    state.set_models(ConnectionKind::AnthropicKey, Vec::new());
    assert!(
        state
            .model_choices()
            .all(|(connection, _)| connection != ConnectionKind::AnthropicKey),
        "an empty report takes the key's catalog off offer"
    );

    let model = |slug: &str| ModelSlug::new(slug).expect("slug");
    assert!(
        !state.send_default_model(ConnectionKind::ChatGpt, model("m-0"), |_| {
            panic!("the saved default isn't sent again")
        })
    );
    let mut sent = Vec::new();
    assert!(
        state.send_default_model(ConnectionKind::ChatGpt, model("m-1"), |op| {
            sent.push(op);
            true
        })
    );
    let sol = choice("m-1", Effort::High, ConnectionKind::ChatGpt);
    assert_eq!(sent, [SettingsOp::SetDefaultModel(sol.clone())]);
    assert_eq!(state.default_model_choice(), Some(&sol));
    assert!(state.model_saving());

    state.error(UserError::from(ErrorKey::SettingsRefused));
    state.set_view(saved(luna.clone()), Update::Answer);
    assert_eq!(state.default_model_choice(), Some(&luna));
    assert!(!state.model_saving());

    let mut sent = Vec::new();
    assert!(
        state.send_default_model(ConnectionKind::ClaudeCode, model("c-0"), |op| {
            sent.push(op);
            true
        })
    );
    assert_eq!(
        sent,
        [SettingsOp::SetDefaultModel(choice(
            "c-0",
            Effort::Low,
            ConnectionKind::ClaudeCode
        ))]
    );

    state.set_models(ConnectionKind::ChatGpt, Vec::new());
    assert_eq!(
        state
            .model_choices()
            .map(|(connection, _)| connection)
            .collect::<Vec<_>>(),
        [ConnectionKind::ClaudeCode]
    );
    let answer = choice("c-0", Effort::Low, ConnectionKind::ClaudeCode);
    state.set_view(saved(answer.clone()), Update::Answer);
    assert_eq!(state.default_model_choice(), Some(&answer));
    assert!(!state.model_saving());
}

#[test]
fn quick_picks_keep_their_connection_until_refusal_or_disconnect() {
    let strings = Strings::english();
    let selected = |state: &Settings| {
        let (rows, checked) = state.default_model_options(&strings);
        checked.map(|index| (rows[index].0, rows[index].1.clone()))
    };
    let mut state = Settings::default();
    let gpt = choice("shared", Effort::Low, ChatGpt);
    let claude = choice("shared", Effort::Low, ClaudeCode);
    let key = choice("key", Effort::Low, AnthropicKey);
    state.set_view(saved(gpt.clone()), Update::Unsolicited);
    state.set_models(ChatGpt, vec![info("shared")]);
    state.set_models(ClaudeCode, vec![info("shared")]);
    state.set_models(AnthropicKey, vec![info("key")]);

    for picked in [&claude, &gpt] {
        let mut offered = None;
        assert!(
            state.send_default_model(picked.connection, picked.model.clone(), |op| {
                offered = Some(op);
                true
            })
        );
        assert_eq!(offered, Some(SettingsOp::SetDefaultModel(picked.clone())));
        assert_eq!(
            selected(&state),
            Some((picked.connection, picked.model.clone()))
        );
    }
    state.set_models(ChatGpt, vec![info("another"), info("shared")]);
    state.set_view(saved(claude.clone()), Update::Unsolicited);
    assert_eq!(selected(&state), Some((ChatGpt, gpt.model.clone())));

    let mut refused = None;
    assert!(
        !state.send_default_model(key.connection, key.model.clone(), |op| {
            refused = Some(op);
            false
        })
    );
    assert_eq!(refused, Some(SettingsOp::SetDefaultModel(key)));
    assert_eq!(selected(&state), Some((ChatGpt, gpt.model.clone())));

    state.set_view(saved(claude.clone()), Update::Answer);
    assert_eq!(selected(&state), Some((ChatGpt, gpt.model.clone())));
    assert!(state.model_saving());
    state.error(UserError::from(ErrorKey::SettingsRefused));
    state.set_view(saved(claude.clone()), Update::Answer);
    assert_eq!(selected(&state), Some((ClaudeCode, claude.model.clone())));
    assert!(!state.model_saving());

    assert!(state.send_default_model(ChatGpt, gpt.model.clone(), |_| true));
    state.disconnected();
    assert_eq!(state.default_model_options(&strings), (Vec::new(), None));
    assert!(!state.model_saving());
    assert!(!state.receive(Some(8), 7, saved(gpt.clone()), Update::Answer));
    state.set_models(ClaudeCode, vec![info("shared")]);
    assert!(state.receive(Some(8), 8, saved(claude.clone()), Update::Unsolicited));
    assert_eq!(selected(&state), Some((ClaudeCode, claude.model)));
    assert!(!state.model_saving());

    state.set_models(ChatGpt, vec![info("shared")]);
    assert!(state.send_default_model(ChatGpt, gpt.model.clone(), |_| true));
    assert!(state.receive(Some(8), 8, saved(gpt.clone()), Update::Answer));
    assert_eq!(selected(&state), Some((ChatGpt, gpt.model)));
    assert!(!state.model_saving());
}

#[test]
fn only_a_current_settings_view_supplies_the_picker_generation() {
    let mut state = Settings::default();
    let view = saved(choice("m-0", Effort::Low, ChatGpt));
    assert_eq!(state.view_generation(), None);
    assert!(state.receive(Some(7), 7, view.clone(), Update::Unsolicited));
    assert!(!state.receive(Some(7), 6, view.clone(), Update::Unsolicited));
    assert_eq!(state.view_generation(), Some(7));
    state.disconnected();
    assert_eq!(state.view_generation(), None);
    assert!(!state.receive(Some(8), 7, view.clone(), Update::Answer));
    assert_eq!(state.view_generation(), None);
    assert!(state.receive(Some(8), 8, view, Update::Unsolicited));
    assert_eq!(state.view_generation(), Some(8));
}

/// The name a notice shows is the newest default's, queued before saved, and a thread's own
/// model is any other model or connection; the effort never makes one.
#[test]
fn the_newest_default_gives_the_notice_its_name_and_marks_a_threads_own_model() {
    let saved_default = choice("m-0", Effort::High, ConnectionKind::ChatGpt);
    let mut state = Settings::default();
    assert_eq!(state.default_model_name(), None);
    assert!(!state.own_model(None));
    // With no default known, a stored choice can't be shown to be the default.
    assert!(state.own_model(Some(&saved_default)));

    state.set_view(saved(saved_default.clone()), Update::Unsolicited);
    // No catalog lists the default yet.
    assert_eq!(state.default_model_name(), None);
    state.set_models(
        ConnectionKind::ChatGpt,
        vec![
            ModelInfo {
                slug: ModelSlug::new("m-0").expect("slug"),
                display_name: ModelName::new("Model Zero").expect("name"),
                supported_efforts: None,
            },
            ModelInfo {
                slug: ModelSlug::new("m-1").expect("slug"),
                display_name: ModelName::new("Model One").expect("name"),
                supported_efforts: None,
            },
        ],
    );
    assert_eq!(
        state.default_model_name().map(ModelName::as_str),
        Some("Model Zero")
    );
    for (stored, own) in [
        (None, false),
        (Some(saved_default.clone()), false),
        (
            Some(choice("m-0", Effort::Low, ConnectionKind::ChatGpt)),
            false,
        ),
        (
            Some(choice("m-1", Effort::High, ConnectionKind::ChatGpt)),
            true,
        ),
        (
            Some(choice("m-0", Effort::High, ConnectionKind::ClaudeCode)),
            true,
        ),
    ] {
        assert_eq!(state.own_model(stored.as_ref()), own, "{stored:?}");
    }

    // A pick the brain hasn't answered is already the newest default.
    assert!(state.send_default_model(
        ConnectionKind::ChatGpt,
        ModelSlug::new("m-1").expect("slug"),
        |_| true
    ));
    assert_eq!(
        state.default_model_name().map(ModelName::as_str),
        Some("Model One")
    );
    assert!(state.own_model(Some(&saved_default)));
    assert!(!state.own_model(Some(&choice("m-1", Effort::Low, ConnectionKind::ChatGpt))));
}
