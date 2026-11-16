//! A headless presence for GPUI tests: its real owner, reducers, typing view and link glue, with
//! no OS-visible window, tray icon, keyboard hook, hotkey or taskbar button, and a link that
//! never connects, so every send it's asked for is refused.

use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::{AppContext as _, Entity, IntoElement, Render, TestAppContext, WindowOptions};
use kumochi_scene::layout::SurfaceGeometry;
use kumochi_scene::theme::Theme;

use super::{
    Activity, Context, Dock, Drawn, EscapeHold, FrameLoop, KumoView, Layout, Line, LinkHandle,
    LinkState, Panel, PanelVisibility, Presence, PresenceNative, Runs, Settings, Strings,
    TextPanel, Threads, Window, platform, presence_labels, pull_dock,
};
use crate::presenter::Presenter;

/// A window root that draws nothing, for the handles presence keeps but these tests never show.
pub(super) struct Blank;

impl Render for Blank {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        gpui_kit::div()
    }
}

/// Presence as `open` builds it, minus every native surface, with its typing panel created and
/// hidden. Its status panel and dock are plain entities behind blank GPUI test windows.
#[expect(
    clippy::too_many_lines,
    reason = "one line per Presence field, as `open` builds it"
)]
pub(super) fn presence(cx: &mut TestAppContext) -> Entity<Presence> {
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_global(Strings::english());
        kumochi_scene::deferred::start().expect("the dropper starts before intake");
        let (events, events_in) = async_channel::unbounded();
        let native = PresenceNative::detached(presence_labels(cx), events.clone())
            .expect("a 1x1 memory mask");
        let mut blank = || {
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| cx.new(|_| Blank))
                .expect("a GPUI test window opens")
                .0
        };
        let (dock_window, panel_window) = (blank(), blank());
        let kumo = cx.new(|_| KumoView::new());
        let panel = cx.new(|_| Panel {
            kumo: kumo.clone(),
            state: LinkState::Connecting,
            voice: None,
            notice: None,
            screen: None,
            screen_line: None,
            stop: false,
            events: events.clone(),
            threads: None,
        });
        let (threads, chat) = Threads::pair();
        let presence = cx.new(|cx| Presence {
            _display_watch: None,
            exclusion: super::exclusion::Exclusion::default(),
            exclusion_timer: None,
            screen_lease: None,
            exclusion_fixture: None,
            input_windows: platform::OverlayWindows::default(),
            native,
            dock_window,
            panel_window,
            dock: cx.new(|_| Dock::new()),
            panel,
            text: None,
            thread_panel: None,
            settings_panel: None,
            onboarding: None,
            onboarding_run: crate::onboarding::ThisRun::Owed,
            onboarding_chatgpt: crate::sign_in_mark::SignInMark::Settled,
            onboarding_claude: crate::sign_in_mark::SignInMark::Settled,
            settings_chatgpt: crate::sign_in_mark::SignInMark::Settled,
            settings_claude: crate::sign_in_mark::SignInMark::Settled,
            settings: cx.new(|_| Settings::default()),
            threads: cx.new(|_| threads),
            chat: cx.new(|_| chat),
            chat_notice: crate::chat_notice::ChatNotice::default(),
            events,
            events_in,
            carry: super::carry::Carry::default(),
            link: LinkHandle::detached(),
            session: None,
            issue_request: 0,
            issue_review: None,
            state: LinkState::Connecting,
            voice: super::voice::Voice::default(),
            activity: Activity::default(),
            runs: Runs::default(),
            paint_scratch: Rc::new(std::cell::RefCell::new(super::paint_stroke::Scratch::new())),
            paint_notice: Rc::default(),
            presenter: Presenter::new(
                SurfaceGeometry::Overlay {
                    default_canvas: [1920.0, 1080.0],
                },
                Arc::new(Theme::default()),
                Box::new(super::paint_cache::Stager(
                    size_of::<super::paint_cache::Stager>()
                        + size_of::<super::overlay::PaintNotice>()
                        + 2 * size_of::<usize>(),
                )),
            ),
            scene_timer: None,
            scene_deadline: None,
            escape_hold: EscapeHold::default(),
            escape_fixture: false,
            layout: Layout::default(),
            drawing_placement: None,
            panel_visibility: PanelVisibility::Hidden,
            scale: 1.0,
            pointer_settle: None,
            line: Line::default(),
            drawn: Drawn::default(),
            kumo,
            pull_dock: pull_dock::PullDock::new(1),
            frames: FrameLoop::default(),
            kumo_frames: FrameLoop::default(),
            overlay: super::overlay_visibility::Visibility::Closed,
            delete_retry: None,
            error: None,
            refusal: None,
        });
        presence.update(cx, |this, cx| {
            this.text = Some(TextPanel::detached(
                this.chat.clone(),
                this.settings.clone(),
                cx,
            ));
        });
        presence
    })
}
