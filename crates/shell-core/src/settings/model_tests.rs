use kumochi_protocol::message::{
    ConnectionKind, Effort, ModelChoice, ModelInfo, ModelName, ModelSlug, ThreadId,
};

use super::{
    AssistantName, DockPosition, ErrorKey, MaxFrameRate, Settings, SettingsOp, SettingsView,
    Update, UserError,
};

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
        shortcut: kumochi_protocol::message::ShortcutView::Default,
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
