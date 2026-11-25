use gpui_kit::base::Root;
use gpui_kit::{Modifiers, OwnedMenuItem, TestAppContext, VisualTestContext, WeakEntity};

use super::*;

fn mount(cx: &mut TestAppContext) -> (AnyWindowHandle, WeakEntity<LocalDock>) {
    let window = cx.update(|cx| {
        gpui_kit::init(cx);
        crate::theme::init(cx);
        cx.set_global(Strings::english());
        open(cx).expect("open the product dock on the test display")
    });
    let owner = window
        .update(cx, |root, _, cx| {
            root.downcast::<Root>()
                .expect("Kit window root")
                .read(cx)
                .view()
                .clone()
                .downcast::<LocalDock>()
                .expect("the real dock owns the window content")
                .downgrade()
        })
        .expect("dock window is open");
    assert_eq!(cx.windows(), [window]);
    (window, owner)
}

#[gpui::test]
fn local_pointer_quits_only_when_the_rendered_button_is_clicked(cx: &mut TestAppContext) {
    let (window, owner) = mount(cx);
    let mut visual = VisualTestContext::from_window(window, cx);
    let status = visual
        .debug_bounds("local-dock-status Can't reach Kumochi's background process.")
        .expect("rendered disconnected status");
    let quit = visual
        .debug_bounds("local-dock-quit")
        .expect("rendered Quit button");

    visual.simulate_click(status.center(), Modifiers::none());
    assert_eq!(cx.windows(), [window]);
    assert!(!cx.read(|cx| {
        owner
            .upgrade()
            .expect("dock owner")
            .read(cx)
            .link
            .is_quitting()
    }));

    visual.simulate_click(quit.center(), Modifiers::none());
    assert!(cx.windows().is_empty());
    assert!(owner.upgrade().is_none());
}

#[gpui::test]
fn the_installed_app_menu_quits_once_without_window_focus(cx: &mut TestAppContext) {
    let (window, owner) = mount(cx);
    let mut visual = VisualTestContext::from_window(window, cx);
    visual.deactivate_window();
    cx.update(|cx| {
        assert_eq!(cx.active_window(), None);
        let menus = cx.get_menus().expect("installed app menu");
        let [menu] = menus.as_slice() else {
            panic!("the dock should install only its app menu");
        };
        assert_eq!(menu.name.as_ref(), DISPLAY_NAME);
        let [OwnedMenuItem::Action { name, action, .. }] = menu.items.as_slice() else {
            panic!("Quit should be the only menu action");
        };
        assert_eq!(name, "Quit");

        cx.dispatch_action(action.as_ref());
        assert!(
            owner
                .upgrade()
                .expect("dock owner")
                .read(cx)
                .link
                .is_quitting()
        );
        cx.dispatch_action(action.as_ref());
    });
    assert!(
        !visual.simulate_close(),
        "native close must defer to the owner"
    );
    assert_eq!(cx.windows(), [window]);

    cx.run_until_parked();
    assert!(cx.windows().is_empty());
    assert!(owner.upgrade().is_none());
}

#[gpui::test]
fn native_window_close_defers_to_the_quit_owner(cx: &mut TestAppContext) {
    let (window, owner) = mount(cx);
    let mut visual = VisualTestContext::from_window(window, cx);
    assert!(
        !visual.simulate_close(),
        "native close must not bypass teardown"
    );
    assert!(cx.read(|cx| {
        owner
            .upgrade()
            .expect("dock owner")
            .read(cx)
            .link
            .is_quitting()
    }));
    assert_eq!(cx.windows(), [window]);

    cx.run_until_parked();
    assert!(cx.windows().is_empty());
    assert!(owner.upgrade().is_none());
}
