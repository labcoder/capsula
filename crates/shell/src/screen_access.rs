//! Kumochi's explained request for macOS Screen Recording access. Dormant: nothing constructs
//! it until the connected Mac shell admits screen capture. macOS decides whether, where and when
//! its dialog shows and when a grant takes effect; nothing here grants or starts capture.

use gpui_kit::component::{
    ActiveTheme as _,
    button::{Button, ButtonVariants as _},
};
use gpui_kit::{
    ClickEvent, Context, FontWeight, Global, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Role, StatefulInteractiveElement as _, Styled as _, Subscription,
    Window, div,
};

use crate::platform::screen_access;
use crate::strings::{Strings, Text};

/// Best effort: `open_url` gives no receipt and macOS may not honor it, so the manual path shows
/// beside it.
const SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";

/// What this launch knows about Screen Recording access. A global, so reopening or re-rendering
/// the view can't ask macOS a second time.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Access {
    granted: bool,
    requested: bool,
}

impl Global for Access {}

/// What one press of the request button did.
#[derive(Debug, PartialEq, Eq)]
enum Press {
    /// Access is already granted or was already requested this launch.
    Ignored,
    /// The window isn't active, so only bringing it forward was asked for.
    Activate,
    /// macOS was asked, once.
    Requested,
}

impl Access {
    /// Takes macOS's nonprompting answer.
    fn recheck(&mut self, preflight: impl FnOnce() -> bool) {
        self.granted = preflight();
    }

    /// Asks only for a separate press in the active window, and at most once per launch.
    /// A false answer is not a denial: the Settings route takes over.
    fn press(
        &mut self,
        window_active: bool,
        focusing_click: bool,
        request: impl FnOnce() -> bool,
    ) -> Press {
        if self.granted || self.requested {
            return Press::Ignored;
        }
        if focusing_click || !window_active {
            return Press::Activate;
        }
        self.requested = true;
        self.granted = request();
        Press::Requested
    }
}

/// The explanation, the one request and the Settings route, in Kumochi's own window.
pub(crate) struct ScreenAccess {
    /// The last press could only ask for this window to come forward.
    forward: bool,
    _activation: Subscription,
}

impl ScreenAccess {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        cx.default_global::<Access>()
            .recheck(screen_access::granted);
        // Coming back, such as from System Settings, rechecks without prompting.
        let activation = cx.observe_window_activation(window, |_, window, cx| {
            if window.is_window_active() {
                cx.global_mut::<Access>().recheck(screen_access::granted);
                cx.notify();
            }
        });
        Self {
            forward: false,
            _activation: activation,
        }
    }

    fn press(&mut self, event: &ClickEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        let active = window.is_window_active();
        self.forward =
            cx.global_mut::<Access>()
                .press(active, event.first_focus(), screen_access::request)
                == Press::Activate;
        if self.forward {
            // Neither call confirms activation; the person presses again.
            cx.activate(false);
            window.activate_window();
        }
        cx.notify();
    }
}

impl Render for ScreenAccess {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let Access { granted, requested } = *cx.global::<Access>();
        let strings = cx.global::<Strings>();
        let title = strings.get(Text::ScreenAccessTitle);
        let status = if granted {
            strings.get(Text::ScreenAccessGranted)
        } else if requested {
            strings.with_app(Text::ScreenAccessPending)
        } else {
            strings.with_app(Text::ScreenAccessExplain)
        };
        let forward = (!granted && !requested && self.forward)
            .then(|| strings.get(Text::ScreenAccessForward));
        let announcement = forward
            .as_ref()
            .map_or_else(|| status.clone(), |hint| format!("{status} {hint}"));
        let request = strings.get(Text::ScreenAccessRequest);
        let settings = strings.get(Text::ScreenAccessOpenSettings);
        let path = strings.get(Text::ScreenAccessPath);

        let mut actions = div().flex().flex_col().gap_2();
        if !granted {
            let mut open = Button::new("screen-access-settings")
                .label(settings)
                .on_click(cx.listener(|_, _, _, cx| cx.open_url(SETTINGS_URL)));
            if requested {
                open = open.primary();
            } else {
                actions = actions.child(
                    Button::new("screen-access-request")
                        .label(request)
                        .primary()
                        .on_click(
                            cx.listener(|this, event, window, cx| this.press(event, window, cx)),
                        ),
                );
                open = open.outline();
            }
            actions = actions.child(open).child(div().text_sm().child(path));
        }

        div()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .flex()
            .flex_col()
            .gap_3()
            .px_4()
            .py_3()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(
                div()
                    .id("screen-access-status")
                    .role(Role::Status)
                    .aria_label(announcement)
                    .child(status)
                    .children(forward.map(|hint| div().child(hint))),
            )
            .child(actions)
    }
}

#[cfg(test)]
mod tests {
    use super::{Access, Press};

    fn never() -> bool {
        panic!("macOS must not be asked")
    }

    #[test]
    fn an_inactive_press_only_asks_to_come_forward_and_the_next_active_press_asks_once() {
        let mut access = Access::default();
        assert_eq!(access.press(false, false, never), Press::Activate);
        assert_eq!(
            access.press(true, true, never),
            Press::Activate,
            "activation before mouse-up must not make the focusing click request"
        );
        assert!(
            !access.requested,
            "bringing the window forward is not a request"
        );
        assert_eq!(access.press(true, false, || false), Press::Requested);
        assert!(
            access.requested && !access.granted,
            "false leaves Settings, not a denial"
        );
    }

    #[test]
    fn a_launch_never_asks_twice_and_a_recheck_does_not_reopen_the_request() {
        let mut access = Access::default();
        assert_eq!(access.press(true, false, || false), Press::Requested);
        access.recheck(|| false);
        assert_eq!(access.press(true, false, never), Press::Ignored);
        assert_eq!(access.press(false, false, never), Press::Ignored);
        access.recheck(|| true);
        assert!(access.granted, "a later nonprompting grant is taken");
    }

    #[test]
    fn granted_access_is_never_requested() {
        let mut access = Access::default();
        access.recheck(|| true);
        assert_eq!(access.press(true, false, never), Press::Ignored);
        assert!(!access.requested, "no request was made");
    }
}
