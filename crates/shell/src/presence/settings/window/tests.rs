use gpui_kit::{AppContext as _, TestAppContext, WindowOptions};

use super::{SettingsPanel, SettingsView};
use crate::presence::test_fixture;

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
