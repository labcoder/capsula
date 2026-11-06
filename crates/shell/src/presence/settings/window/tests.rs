use gpui_kit::{AppContext as _, TestAppContext, WindowOptions};

use super::{SettingsPanel, SettingsView};
use crate::presence::test_fixture;

#[gpui::test]
fn recovery_settings_clicks_open_the_exact_tab_through_the_hidden_panel_path(
    cx: &mut TestAppContext,
) {
    use crate::chat_notice::Source;
    use kumochi_protocol::message::{Action, ErrorKey, UserError};

    for (key, source, expected) in [
        (ErrorKey::AnthropicKeyRejected, Source::Account, 1),
        (ErrorKey::SettingsRefused, Source::Settings(None), 0),
    ] {
        let owner = test_fixture::presence(cx);
        let (window, view) = owner.update(cx, |owner, cx| {
            let model = owner.settings.clone();
            let weak = cx.weak_entity();
            let events = owner.events.clone();
            let (window, view) =
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| SettingsView::new(model, weak, events, window, cx))
                })
                .expect("headless settings window");
            owner.settings_panel = Some(SettingsPanel {
                window,
                view: view.clone(),
                visible: false,
                focused: false,
                bounds: [0; 4],
                previous: None,
                activation: None,
            });
            owner.chat_notice.present(
                UserError::new(key, Vec::new(), vec![Action::OpenSettings])
                    .expect("bounded settings recovery"),
                source,
            );
            owner.refresh_conversation(cx);
            assert!(!owner.settings_open());
            (owner.text.as_ref().expect("typing fixture").window, view)
        });
        cx.run_until_parked();
        {
            let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
            let bounds = frame
                .debug_bounds("chat-recovery-action-0")
                .expect("Settings recovery control");
            frame.simulate_click(bounds.center(), gpui_kit::Modifiers::default());
        }
        cx.run_until_parked();
        owner.read_with(cx, |owner, cx| {
            assert!(owner.settings_open());
            assert_eq!(view.read(cx).tab, expected);
        });
    }
}

#[gpui::test]
fn recovery_usage_click_opens_the_public_usage_destination(cx: &mut TestAppContext) {
    use crate::chat_notice::Source;
    use kumochi_protocol::message::{Action, ErrorKey, MANAGE_USAGE_URL, UserError};

    let owner = test_fixture::presence(cx);
    let window = owner.update(cx, |owner, cx| {
        owner.chat_notice.present(
            UserError::new(ErrorKey::UsageLimit, Vec::new(), vec![Action::ManageUsage])
                .expect("bounded usage recovery"),
            Source::Account,
        );
        owner.refresh_conversation(cx);
        owner.text.as_ref().expect("typing fixture").window
    });
    cx.run_until_parked();
    assert_eq!(cx.opened_url(), None);
    {
        let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
        let bounds = frame
            .debug_bounds("chat-recovery-action-0")
            .expect("Manage usage link");
        frame.simulate_click(bounds.center(), gpui_kit::Modifiers::default());
    }
    cx.run_until_parked();
    assert_eq!(cx.opened_url().as_deref(), Some(MANAGE_USAGE_URL));
}

#[gpui::test]
fn recovery_openers_select_the_exact_tab_without_toggling_a_visible_window_closed(
    cx: &mut TestAppContext,
) {
    let owner = test_fixture::presence(cx);
    owner.update(cx, |owner, cx| {
        let model = owner.settings.clone();
        let weak = cx.weak_entity();
        let events = owner.events.clone();
        let (window, view) = gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| SettingsView::new(model, weak, events, window, cx))
        })
        .expect("headless settings window");
        owner.settings_panel = Some(SettingsPanel {
            window,
            view: view.clone(),
            visible: true,
            focused: false,
            bounds: [0; 4],
            previous: None,
            activation: None,
        });
        owner.open_accounts(cx).expect("accounts tab");
        assert!(owner.settings_open());
        assert_eq!(view.read(cx).tab, 1);
        owner.open_accounts(cx).expect("accounts stays open");
        assert!(owner.settings_open());
        assert_eq!(view.read(cx).tab, 1);
        owner.open_general(cx).expect("general tab");
        assert!(owner.settings_open());
        assert_eq!(view.read(cx).tab, 0);
        owner.open_general(cx).expect("general stays open");
        assert!(owner.settings_open());
        assert_eq!(view.read(cx).tab, 0);
    });
}
