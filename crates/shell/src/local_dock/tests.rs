use std::sync::{Arc, Mutex, Weak};

use gpui_kit::base::Root;
use gpui_kit::{Modifiers, OwnedMenuItem, TestAppContext, VisualTestContext, WeakEntity};

use super::*;

/// An admission that records whether it was dropped on GPUI's main thread.
struct Probe {
    executor: gpui_kit::BackgroundExecutor,
    on_main: Arc<Mutex<Option<bool>>>,
    _alive: Arc<()>,
}

impl Drop for Probe {
    fn drop(&mut self) {
        *self.on_main.lock().unwrap() = Some(self.executor.is_main_thread());
    }
}

fn custody(cx: &TestAppContext) -> (Rc<Custody>, Weak<()>, Arc<Mutex<Option<bool>>>) {
    let alive = Arc::new(());
    let held = Arc::downgrade(&alive);
    let on_main = Arc::new(Mutex::new(None));
    let probe = Probe {
        executor: cx.executor(),
        on_main: Arc::clone(&on_main),
        _alive: alive,
    };
    (Custody::new(probe), held, on_main)
}

#[gpui::test]
fn failed_open_releases_admission_before_returning_to_the_quit_caller(cx: &mut TestAppContext) {
    let display = cx.read(|cx| cx.primary_display().expect("the test display").id());
    cx.simulate_display_removed(display);
    let (custody, held, on_main) = custody(cx);

    let opened = cx.update(|cx| open_with_rollback(cx, &custody));
    cx.run_until_parked();

    assert!(matches!(opened, Err(OpenError::Display)));
    assert_eq!(
        *on_main.lock().unwrap(),
        Some(false),
        "the rollback drop runs off the UI thread"
    );
    assert!(
        held.upgrade().is_none(),
        "the failure hands back no live lock"
    );
    assert!(cx.windows().is_empty());
}

fn mount(cx: &mut TestAppContext) -> (AnyWindowHandle, WeakEntity<LocalDock>) {
    mount_with(cx, Custody::new(()))
}

/// Opens the dock and gives it the only strong custody, as `run` does.
fn mount_with(
    cx: &mut TestAppContext,
    custody: Rc<Custody>,
) -> (AnyWindowHandle, WeakEntity<LocalDock>) {
    let window = cx.update(|cx| {
        gpui_kit::init(cx);
        crate::theme::init(cx);
        cx.set_global(Strings::english());
        open(cx, &custody).expect("open the product dock on the test display")
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
    let (custody, held, on_main) = custody(cx);
    let (window, owner) = mount_with(cx, custody);
    let mut visual = VisualTestContext::from_window(window, cx);
    let status = visual
        .debug_bounds("local-dock-status Can't reach Kumochi's background process.")
        .expect("rendered disconnected status");
    let quit = visual
        .debug_bounds("local-dock-quit")
        .expect("rendered Quit button");

    visual.simulate_click(status.center(), Modifiers::none());
    assert_eq!(cx.windows(), [window]);
    assert!(
        held.upgrade().is_some(),
        "the open dock keeps its admission"
    );
    assert!(!cx.read(|cx| {
        owner
            .upgrade()
            .expect("dock owner")
            .read(cx)
            .custody
            .link
            .borrow()
            .as_ref()
            .expect("the open dock's link")
            .is_quitting()
    }));

    visual.simulate_click(quit.center(), Modifiers::none());
    cx.run_until_parked();
    assert!(cx.windows().is_empty());
    assert!(owner.upgrade().is_none());
    assert!(held.upgrade().is_none(), "Quit releases the admission");
    assert_eq!(
        *on_main.lock().unwrap(),
        Some(false),
        "the Quit drop runs off the UI thread"
    );
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
                .custody
                .link
                .borrow()
                .as_ref()
                .expect("the open dock's link")
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
            .custody
            .link
            .borrow()
            .as_ref()
            .expect("the open dock's link")
            .is_quitting()
    }));
    assert_eq!(cx.windows(), [window]);

    cx.run_until_parked();
    assert!(cx.windows().is_empty());
    assert!(owner.upgrade().is_none());
}

#[gpui::test]
fn losing_the_view_keeps_the_admission_for_a_later_quit(cx: &mut TestAppContext) {
    let (custody, held, on_main) = custody(cx);
    let (window, owner) = mount_with(cx, custody);

    window
        .update(cx, |_, window, _| window.remove_window())
        .expect("dock window is open");
    cx.run_until_parked();
    assert!(owner.upgrade().is_none(), "the view is gone");
    assert!(
        held.upgrade().is_some(),
        "the app still holds the admission"
    );
    assert_eq!(
        *on_main.lock().unwrap(),
        None,
        "nothing dropped it on the UI thread"
    );

    cx.update(|cx| cx.dispatch_action(&QuitLocalDock));
    cx.run_until_parked();
    assert!(
        held.upgrade().is_none(),
        "Quit without a window releases the admission"
    );
    assert_eq!(
        *on_main.lock().unwrap(),
        Some(false),
        "the release runs off the UI thread"
    );
}
