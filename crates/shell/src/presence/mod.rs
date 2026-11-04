//! The resting dock line, transient status panel, typing panel and native tray share one
//! UI-thread owner, which also applies keyboard gestures and Esc.

mod accounts_view;
mod deletes;
pub(crate) mod diagnostics;
mod drag;
mod drawings;
mod escape;
mod frame_rate;
mod geometry;
mod gesture;
mod kumo;
mod kumo_view;
mod launch;
mod line;
mod link;
pub(crate) mod observer;
mod paint;
mod panels;
mod pull_dock;
mod pull_picture;
mod pulling;
mod settings;
mod settings_view;
#[cfg(test)]
mod tests;
mod text;
mod thread_selection;
mod thread_view;
mod threads;

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender};
use gpui_kit::component::input::InputState;
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, Global, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Task, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, point, px, size,
};
use kumochi_protocol::constants;

use crate::escape::{Activity, Escape, EscapeHold, Panel as EscapePanel};
use crate::input::{Effect, Gesture};
use crate::link::{LinkEvent, LinkHandle, LinkState, Session};
use crate::platform::{
    self, Capture, EscapeHotkey, EventTime, Foreground, PresenceEvent, PresenceLabels,
    PresenceNative, PresenceSurface,
};
use crate::runs::Runs;
use crate::settings::Settings;
use crate::strings::{Strings, Text};
use crate::threads::Threads;
pub(crate) use diagnostics::Diagnostics;
use frame_rate::FrameLoop;
use geometry::{Layout, PanelVisibility, Position};
use kumo::Mood;
use kumo_view::KumoView;
pub(crate) use launch::Launch;
use line::{Dock, Line, LineFrame, Placement};
pub(crate) use observer::Observer;
use observer::{Drawn, Surface};
use paint::{Stamp, Target};
use settings::SettingsPanel;
use text::TextPanel;
use threads::ThreadsPanel;

/// Keeps the app owner alive even when its panel is hidden.
struct PresenceApp {
    _owner: Entity<Presence>,
}
impl Global for PresenceApp {}

struct Presence {
    native: PresenceNative,
    dock_window: AnyWindowHandle,
    panel_window: AnyWindowHandle,
    dock: Entity<Dock>,
    panel: Entity<Panel>,
    /// Created on the first double tap, then hidden and reused so its draft survives.
    text: Option<TextPanel>,
    thread_panel: Option<ThreadsPanel>,
    settings_panel: Option<SettingsPanel>,
    settings: Entity<Settings>,
    threads: Entity<Threads>,
    events: Sender<PresenceEvent>,
    link: LinkHandle,
    /// The connection the UI last applied `Connected` for, until its next state.
    session: Option<Session>,
    state: LinkState,
    /// The canonical gesture; the Raw Input callback reduces it before GPUI sees anything.
    gesture: Rc<Cell<Gesture>>,
    /// What `runs` and the typing panel last made Esc see.
    activity: Activity,
    runs: Runs,
    presenter: crate::presenter::Presenter,
    scene_timer: Option<Task<()>>,
    scene_deadline: Option<Instant>,
    escape_hold: EscapeHold<EscapeHotkey>,
    /// `--esc-fixture`: hold global Esc from startup until the first Esc, with no fake work.
    escape_fixture: bool,
    layout: Layout,
    panel_visibility: PanelVisibility,
    scale: f32,
    pointer_settle: Option<Task<()>>,
    /// The dock's placement as Windows accepted it.
    line: Line,
    /// The observer's state after the last accepted mask upload, for the dock's paint stamp.
    drawn: Drawn,
    /// Kumo in the peek.
    kumo: Entity<KumoView>,
    pull_dock: pull_dock::PullDock,
    /// The one source of animation frames.
    frames: FrameLoop,
    /// Retries deletes the control queue had no room for.
    delete_retry: Option<Task<()>>,
    /// The brain's last error, until a turn is sent or its field's retry succeeds.
    error: Option<SharedString>,
    /// A local submission refusal, shown only in the typing panel.
    refusal: Option<Text>,
}

impl Presence {
    fn position(
        &mut self,
        position: Position,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        // shortcut: work-area changes need another position choice; slice 10 adds live settings updates.
        let area = platform::work_area()?;
        let physical = |value: f32| scaled(value, self.scale);
        // Kumo and its gap sit beside the 300 px the details had before the peek; the peek is
        // only as tall as its content.
        let layout = position.layout(
            area,
            line::slot(self.scale),
            [physical(380.0), physical(PEEK_HEIGHT)],
            0,
        );
        let index = Position::ALL
            .iter()
            .position(|candidate| *candidate == position)
            .expect("every dock position has a menu entry");
        let frame = LineFrame::in_slot(layout.dock, self.scale);
        self.request_line(
            Placement {
                layout,
                frame,
                index,
            },
            cx,
        )?;
        self.dismiss_settings(cx)?;
        self.dismiss_text(cx)?;
        self.dismiss_threads(cx)?;
        self.set_panel(PanelVisibility::Hidden, cx)?;
        if let Err(error) = self.pointer_changed(cx) {
            tracing::debug!(%error, "can't recheck the pointer after moving the dock");
        }
        Ok(())
    }

    fn set_panel(
        &mut self,
        visibility: PanelVisibility,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        self.set_panel_from(visibility, None, cx)
    }

    fn set_panel_from(
        &mut self,
        visibility: PanelVisibility,
        trigger_qpc: Option<i64>,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        observer::hover(cx, visibility, trigger_qpc);
        self.panel_window
            .update(cx, |_, window, cx| {
                if visibility == PanelVisibility::Visible {
                    platform::place_presence_window(window, self.layout.panel)?;
                } else {
                    platform::hide_presence_window(window)?;
                }
                window.bounds_changed(cx);
                Ok::<_, platform::PresenceError>(())
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        if visibility != self.panel_visibility {
            let motion = kumo_view::motion(cx);
            self.kumo.update(cx, |kumo, cx| match visibility {
                PanelVisibility::Visible => kumo.show(motion, cx),
                PanelVisibility::Hidden => kumo.hide(cx),
            });
        }
        self.panel_visibility = visibility;
        Ok(())
    }

    fn pointer_changed(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        self.pointer_moved(None, cx)
    }

    /// `entered_qpc` is the native pointer-entry counter the gate times the peek from.
    fn pointer_moved(
        &mut self,
        entered_qpc: Option<i64>,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let visibility = if self.line.contains(platform::cursor_position()?) {
            panels::over_line(self.panel_owner(cx))
        } else {
            panels::pointer(self.panel_visibility, self.panel_owner(cx))
        };
        if visibility != self.panel_visibility {
            self.set_panel_from(visibility, entered_qpc, cx)?;
        }
        if self.panel_owner(cx) != panels::Owner::Pointer {
            self.pointer_settle = None;
            return Ok(());
        }
        let point = platform::cursor_position()?;
        if self.hovered(point, self.panel_visibility) {
            self.pointer_settle = None;
            return Ok(());
        }
        if self.panel_visibility == PanelVisibility::Hidden {
            return Ok(());
        }
        self.pointer_settle = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            let result = this.update(cx, |this, cx| {
                let point = platform::cursor_position()?;
                if !this.hovered(point, PanelVisibility::Visible) {
                    let visibility = panels::left(this.panel_owner(cx));
                    if !this.text_open() && visibility != this.panel_visibility {
                        this.set_panel(visibility, cx)?;
                    }
                }
                Ok::<_, OpenError>(())
            });
            if let Ok(Err(error)) = result {
                tracing::error!(%error, "can't dismiss the status panel");
            }
        }));
        Ok(())
    }

    fn text_open(&self) -> bool {
        self.text.as_ref().is_some_and(|text| text.typing.visible)
    }

    /// Whether the pointer is over the line's band, where the hit proxy takes it, or over the
    /// panel when it's shown.
    fn hovered(&self, point: [i32; 2], panel: PanelVisibility) -> bool {
        self.line.contains(point) || self.layout.panel_contains(point, panel)
    }

    /// Shows Listening on the line, and the settings notice in the status panel.
    // shortcut: the line carries no notice, so the offline and settings notices show only in the status panel; B2b's peek shows them by the line.
    fn refresh_dock(&self, cx: &mut Context<'_, Self>) {
        let listening = self.gesture.get().listening();
        let was_listening = self.dock.update(cx, |dock, cx| {
            cx.notify();
            std::mem::replace(&mut dock.listening, listening)
        });
        if listening != was_listening {
            let mood = if listening {
                Mood::Listening
            } else {
                Mood::Idle
            };
            let motion = kumo_view::motion(cx);
            self.kumo
                .update(cx, |kumo, cx| kumo.set_mood(mood, motion, cx));
        }
        let notice = self.settings.read(cx).dock_notice().filter(|notice| {
            *notice == Text::SettingsNotSaved || self.state == LinkState::Connected
        });
        self.panel.update(cx, |panel, cx| {
            if panel.notice != notice {
                panel.notice = notice;
                cx.notify();
            }
        });
    }

    /// Applies gesture effects in order. `at` is the native callback's time.
    fn apply_effects(
        &mut self,
        effects: [Option<Effect>; 3],
        at: EventTime,
        dock: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        for effect in effects.into_iter().flatten() {
            match effect {
                Effect::Listening => {
                    self.refresh_dock(cx);
                    // A deferred Listening the press already outran shows nothing, so isn't timed.
                    if self.gesture.get().listening() {
                        let applied_us = at.instant.elapsed().as_micros();
                        let start_qpc = at.qpc;
                        diagnostics::record(
                            cx,
                            format_args!("listening applied_us={applied_us} start_qpc={start_qpc}"),
                        );
                    }
                }
                Effect::Resting => {
                    self.refresh_dock(cx);
                    diagnostics::record(cx, format_args!("resting"));
                }
                Effect::HoldStarted => self.set_capture(Capture::Excluded, dock, cx),
                Effect::HoldEnded => self.set_capture(Capture::Allowed, dock, cx),
                Effect::OpenInput => {
                    if let Err(error) = self.open_text(cx) {
                        tracing::error!(%error, "can't open the typing panel");
                    }
                }
            }
            if cx.has_global::<Diagnostics>() {
                let gesture = self.gesture.get();
                self.dock.update(cx, |dock, _| {
                    dock.pending_paint.follow(effect, &gesture, at.instant);
                });
            }
        }
    }

    /// Hides every Kumochi surface from capture before the hold is announced, and shows them
    /// again when it ends. Slice 6 captures and slice 7 sends `HoldRecognized` after this.
    fn set_capture(&self, capture: Capture, dock: &Window, cx: &mut Context<'_, Self>) {
        let mut all = true;
        if let Err(error) = platform::set_capture(dock, capture) {
            tracing::warn!(%error, "can't change the dock's capture exclusion");
            all = false;
        }
        if let Err(error) = self.native.set_capture(capture) {
            tracing::warn!(%error, "can't change the dock proxy's capture exclusion");
            all = false;
        }
        let text = self.text.as_ref().map(|text| text.window);
        let threads = self.thread_panel.as_ref().map(|panel| panel.window);
        let settings = self.settings_panel.as_ref().map(|panel| panel.window);
        for window in [Some(self.panel_window), text, threads, settings]
            .into_iter()
            .flatten()
        {
            match window.update(cx, |_, window, _| platform::set_capture(window, capture)) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(%error, "can't change a panel's capture exclusion");
                    all = false;
                }
                Err(error) => {
                    tracing::warn!(%error, "a panel is gone");
                    all = false;
                }
            }
        }
        match capture {
            Capture::Excluded => {
                tracing::info!("hold recognized after capture exclusion");
                diagnostics::record(cx, format_args!("hold excluded={all}"));
            }
            Capture::Allowed => diagnostics::record(cx, format_args!("hold ended restored={all}")),
        }
    }

    fn activity(
        runs: &Runs,
        panel: EscapePanel,
        _presenter: &crate::presenter::Presenter,
    ) -> Activity {
        // shortcut: B5 adds painted v1 scenes to global Esc activity; mirrored scenes aren't visible.
        Activity {
            working: runs.working(),
            speaking: false,
            drawing: runs.drawing() && matches!(panel, EscapePanel::Open),
            pulling: false,
        }
    }

    /// Follows a change in the runs or the typing panel: holds or releases Esc if what Esc sees
    /// changed, and shows the newest run.
    fn runs_changed(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        self.refresh_conversation(cx);
        let panel = if self.text_open() {
            EscapePanel::Open
        } else {
            EscapePanel::Closed
        };
        let activity = Activity {
            pulling: self.kumo.read(cx).pull_live(),
            ..Self::activity(&self.runs, panel, &self.presenter)
        };
        if activity == self.activity {
            return Ok(());
        }
        self.activity = activity;
        self.sync_escape(cx)
    }

    /// Holds or releases global Esc to match what's moving, showing Stop at once if Windows
    /// refuses, in the status panel and in an open typing panel.
    fn sync_escape(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        let wanted = self.activity.holds_global_escape() || self.escape_fixture;
        if wanted {
            let native = &self.native;
            self.escape_hold.acquire(|| native.register_escape());
        } else {
            self.escape_hold.release();
        }
        let stop = self.escape_hold.shows_stop();
        diagnostics::record(cx, format_args!("escape wanted={wanted} stop={stop}"));
        let changed = self.panel.update(cx, |panel, cx| {
            let changed = panel.stop != stop;
            if changed {
                panel.stop = stop;
                cx.notify();
            }
            changed
        });
        if let Some(text) = &self.text {
            text.view.update(cx, |view, cx| {
                if view.stop != stop {
                    view.stop = stop;
                    cx.notify();
                }
            });
        }
        if let Some(panel) = &self.thread_panel {
            panel.view.update(cx, |view, cx| {
                if view.stop != stop {
                    view.stop = stop;
                    cx.notify();
                }
            });
        }
        if let Some(panel) = &self.settings_panel {
            panel.view.update(cx, |view, cx| {
                view.stop = stop;
                cx.notify();
            });
        }
        if changed {
            self.pointer_changed(cx)?;
        }
        Ok(())
    }

    /// `--gate-observer` only: starts observing the proxy and records where everything is.
    fn observe(&self, position: Position, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        if !cx.has_global::<Observer>() {
            return Ok(());
        }
        let dock_hwnd = self
            .dock_window
            .update(cx, |_, window, _| platform::window_handle(window))
            .map_err(|error| OpenError::Gpui(error.into()))??;
        let panel_hwnd = self
            .panel_window
            .update(cx, |_, window, _| platform::window_handle(window))
            .map_err(|error| OpenError::Gpui(error.into()))??;
        let startup = observer::Startup {
            dock_hwnd,
            panel_hwnd,
            scale: self.scale,
            position,
            work_area: platform::work_area()?,
            dock: self.layout.dock,
            overlay: self.layout.overlay,
            panel: self.layout.panel,
        };
        observer::start(cx, &self.native, self.events.clone(), startup);
        self.dock_window
            .update(cx, |_, window, _| {
                window.on_next_frame(|_, cx| observer::flush(cx, Surface::Dock));
            })
            .map_err(|error| OpenError::Gpui(error.into()))?;
        self.panel_window
            .update(cx, |_, window, _| {
                window.on_next_frame(|_, cx| observer::flush(cx, Surface::Panel));
            })
            .map_err(|error| OpenError::Gpui(error.into()))?;
        Ok(())
    }

    fn quit(&mut self, cx: &mut Context<'_, Self>) {
        if !self.link.begin_quit() {
            return;
        }
        self.end_pull(cx);
        self.presenter.disconnected();
        self.scene_timer = None;
        self.scene_deadline = None;
        let link = self.link.clone();
        diagnostics::settled_paints(cx);
        let ([reshades, unchanged], shading) = self.kumo.read(cx).totals();
        diagnostics::record(
            cx,
            format_args!(
                "kumo reshades={reshades} unchanged={unchanged} shade_us={}",
                shading.as_micros()
            ),
        );
        let diagnostics = diagnostics::release(cx);
        let observer = observer::release(cx);
        let told = cx.background_executor().spawn(async move {
            link.finish_quit();
            drop(diagnostics);
            if let Some(observer) = observer {
                observer.finish();
            }
        });
        cx.spawn(async move |_, cx| {
            told.await;
            cx.update(|cx| {
                drop(cx.remove_global::<PresenceApp>());
                cx.quit();
            });
        })
        .detach();
    }
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::as_conversions,
    reason = "bounded UI dimensions rounded to native physical pixels"
)]
fn scaled(value: f32, scale: f32) -> i32 {
    (value * scale).round() as i32
}

struct Panel {
    kumo: Entity<KumoView>,
    state: LinkState,
    /// A settings notice the dock line can't show.
    notice: Option<Text>,
    /// Shown only when Esc is wanted globally but Windows refused it.
    stop: bool,
    events: Sender<PresenceEvent>,
    threads: Option<WeakEntity<Presence>>,
}

impl Render for Panel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let chat_owner = self.threads.clone();
        let threads_owner = self.threads.clone();
        let settings_owner = self.threads.clone();
        let retry = (self.state == LinkState::Failed).then(|| {
            let events = self.events.clone();
            Button::new("retry")
                .small()
                .label(strings.get(Text::Retry))
                .on_click(move |_, _, _| {
                    send(&events, PresenceEvent::Retry);
                })
        });
        let stop = self.stop.then(|| {
            let events = self.events.clone();
            Target(
                Button::new("stop")
                    .small()
                    .label(strings.get(Text::Stop))
                    .on_click(move |_, _, _| {
                        send(&events, PresenceEvent::Escape);
                    })
                    .into_element(),
            )
        });
        let stamped = cx.has_global::<Observer>() || cx.has_global::<Diagnostics>();
        // Kumo, the name and the status read as one identity; size and weight carry the
        // hierarchy, since muted text fails AA contrast over the glass.
        let identity = div()
            .flex()
            .items_center()
            .gap_3()
            .child(self.kumo.clone())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .min_w_0()
                    .child(
                        div()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(constants::DISPLAY_NAME),
                    )
                    .child(div().text_sm().child(strings.get(status_text(self.state))))
                    .children(
                        self.notice
                            .map(|notice| div().text_sm().child(strings.get(notice))),
                    ),
            );
        // Chat leads; Tasks and Settings stay quiet. Quit lives in the tray.
        let actions = div()
            .flex()
            .flex_wrap()
            .gap_1()
            .children(stop)
            .children(retry)
            .child(
                action(
                    "chat",
                    strings.get(Text::Chat),
                    chat_owner,
                    Presence::open_text,
                )
                .primary(),
            )
            .child(
                action(
                    "threads",
                    strings.get(Text::OpenThreads),
                    threads_owner,
                    Presence::open_threads,
                )
                .ghost(),
            )
            .child(
                action(
                    "settings",
                    strings.get(Text::OpenSettings),
                    settings_owner,
                    Presence::toggle_settings,
                )
                .ghost(),
            );
        // The theme fill over the blur, so the peek stays readable where glass isn't drawn.
        div()
            .size_full()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover.opacity(0.85))
            .text_color(cx.theme().popover_foreground)
            .flex()
            .flex_col()
            .gap_2()
            .children(stamped.then_some(Stamp(Surface::Panel, Drawn::default())))
            .child(identity)
            .child(actions)
    }
}

/// A peek button that opens one of presence's panels on `owner` once the click is handled.
fn action(
    id: &'static str,
    label: String,
    owner: Option<WeakEntity<Presence>>,
    open: fn(&mut Presence, &mut Context<'_, Presence>) -> Result<(), OpenError>,
) -> Button {
    Button::new(id)
        .small()
        .label(label)
        .on_click(move |_, _, cx| {
            if let Some(owner) = owner.clone() {
                cx.defer(move |cx| {
                    if let Ok(Err(error)) = owner.update(cx, open) {
                        tracing::error!(%error, panel = id, "can't open a panel from the peek");
                    }
                });
            }
        })
}

fn send(events: &Sender<PresenceEvent>, event: PresenceEvent) {
    if events.try_send(event).is_err() {
        tracing::error!("presence event receiver closed");
    }
}

fn status_text(state: LinkState) -> Text {
    match state {
        LinkState::Connecting => Text::Connecting,
        LinkState::Connected => Text::Connected,
        LinkState::Reconnecting => Text::Reconnecting,
        LinkState::Failed => Text::Failed,
    }
}

/// A native surface or GPUI window couldn't be initialized or placed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OpenError {
    #[error("GPUI couldn't create or update a presence surface")]
    Gpui(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error(transparent)]
    Native(#[from] platform::PresenceError),
    #[error("--quit-after-ms must be between 1 and 120000")]
    QuitDelay,
    #[error("--esc-fixture needs --quit-after-ms or --smoke, so global Esc is always given back")]
    EscFixtureDeadline,
    #[error("--gate-observer and --receipt-fixture need --quit-after-ms or --smoke")]
    ObserverDeadline,
    #[error("--receipt-fixture needs --gate-observer and comes at most once")]
    ReceiptFixture,
    #[error("the scene dropper thread couldn't start")]
    Dropper(#[source] std::io::Error),
}

fn options() -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(0.0), px(0.0)),
            size(px(112.0), px(48.0)),
        ))),
        titlebar: None,
        focus: false,
        show: false,
        kind: WindowKind::PopUp,
        window_background: WindowBackgroundAppearance::Transparent,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        inactive_frame_interval: None,
        ..Default::default()
    }
}

fn configure_surface(
    window: &mut Window,
    surface: PresenceSurface,
    events: Sender<PresenceEvent>,
    cx: &mut App,
) -> Result<(), platform::PresenceError> {
    platform::configure_presence_window(window, surface, events)?;
    gpui_kit::base::Root::update(window, cx, |root, _, cx| {
        root.style().background = Some(gpui_kit::transparent_black().into());
        cx.notify();
    });
    Ok(())
}

fn presence_labels(cx: &App) -> PresenceLabels {
    let strings = cx.global::<Strings>();
    PresenceLabels {
        tooltip: constants::DISPLAY_NAME.to_owned(),
        position_menu: strings.get(Text::DockPosition),
        positions: Text::POSITIONS.map(|text| strings.get(text)),
        retry: strings.get(Text::Retry),
        quit: strings.get(Text::Quit),
    }
}

fn status_window(
    events: &Sender<PresenceEvent>,
    kumo: Entity<KumoView>,
    cx: &mut App,
) -> Result<(AnyWindowHandle, Entity<Panel>), OpenError> {
    let panel_events = events.clone();
    let close_events = events.clone();
    let glass = WindowOptions {
        window_background: WindowBackgroundAppearance::Blurred,
        ..options()
    };
    let (panel_window, panel) = gpui_kit::open_window(glass, cx, |window, cx| {
        window.set_window_title(constants::DISPLAY_NAME);
        window.on_window_should_close(cx, move |_, _| {
            send(&close_events, PresenceEvent::DismissPanel);
            false
        });
        cx.new(|_| Panel {
            kumo,
            state: LinkState::Connecting,
            notice: None,
            stop: false,
            events: panel_events,
            threads: None,
        })
    })
    .map_err(|error| OpenError::Gpui(error.into()))?;
    let status_events = events.clone();
    panel_window
        .update(cx, |_, window, cx| {
            configure_surface(window, PresenceSurface::Panel, status_events, cx)
        })
        .map_err(|error| OpenError::Gpui(error.into()))??;
    Ok((panel_window, panel))
}

/// Opens a small nonactivating dock immediately and arms the keyboard, so Listening works
/// from the first frame. The caller starts the brain link afterwards.
pub(crate) fn open(
    states: Receiver<LinkEvent>,
    link: LinkHandle,
    flags: &Launch,
    diagnostics: Option<Diagnostics>,
    observer: Option<Observer>,
    cx: &mut App,
) -> Result<(), OpenError> {
    if let Some(diagnostics) = diagnostics {
        cx.set_global(diagnostics);
    }
    if let Some(observer) = observer {
        cx.set_global(observer);
    }
    let (events, event_changes) = async_channel::unbounded();
    let labels = presence_labels(cx);
    let dock_events = events.clone();
    let (dock_window, dock) = gpui_kit::open_window(options(), cx, |window, cx| {
        window.set_window_title(constants::DISPLAY_NAME);
        window.on_window_should_close(cx, |_, _| false);
        cx.new(|_| Dock::new())
    })
    .map_err(|error| OpenError::Gpui(error.into()))?;
    let scale = dock_window
        .update(cx, |_, window, cx| {
            configure_surface(window, PresenceSurface::Overlay, dock_events, cx)?;
            Ok::<_, platform::PresenceError>(window.scale_factor())
        })
        .map_err(|error| OpenError::Gpui(error.into()))??;
    let mask = pull_picture::largest_area(line::slot(scale), scale);
    let native = PresenceNative::new(labels, events.clone(), mask)?;
    let kumo = cx.new(|_| KumoView::new());
    let (panel_window, panel) = status_window(&events, kumo.clone(), cx)?;
    let gesture = Rc::new(Cell::new(Gesture::default()));
    let threads = cx.new(|_| Threads::default());
    let settings = cx.new(|_| Settings::default());
    // shortcut: B5 owns per-display geometry and refresh; preparation uses a default if none is primary.
    let logical_size = cx.primary_display().map_or([1920.0, 1080.0], |display| {
        let display_size = display.bounds().size;
        [display_size.width.into(), display_size.height.into()]
    });
    kumochi_scene::deferred::start().map_err(OpenError::Dropper)?;
    let presenter = crate::presenter::Presenter::new(
        kumochi_scene::layout::SurfaceGeometry::Overlay { logical_size },
        std::sync::Arc::new(kumochi_scene::theme::Theme::default()),
    );
    let owner = cx.new(|cx| {
        listen(states, event_changes, cx);
        Presence {
            native,
            dock_window,
            panel_window,
            dock,
            panel,
            text: None,
            thread_panel: None,
            settings_panel: None,
            settings,
            threads,
            events,
            link,
            session: None,
            state: LinkState::Connecting,
            gesture: Rc::clone(&gesture),
            activity: Activity::default(),
            runs: Runs::default(),
            presenter,
            scene_timer: None,
            scene_deadline: None,
            escape_hold: EscapeHold::default(),
            escape_fixture: flags.escape_fixture,
            layout: Layout::default(),
            panel_visibility: PanelVisibility::Hidden,
            scale,
            pointer_settle: None,
            delete_retry: None,
            error: None,
            refusal: None,
            line: Line::default(),
            drawn: Drawn::default(),
            kumo: kumo.clone(),
            pull_dock: pull_dock::PullDock::new(usize::try_from(mask[1]).unwrap_or(0)),
            frames: FrameLoop::default(),
        }
    });
    owner.update(cx, |this, cx| {
        this.follow_kumo(cx);
        this.kumo.update(cx, |kumo, cx| kumo.rebake(scale, cx));
    });
    let status_panel = owner.read(cx).panel.clone();
    status_panel.update(cx, |panel, _| panel.threads = Some(owner.downgrade()));
    let position = flags.position();
    owner.update(cx, |this, cx| this.position(position, cx))?;
    owner.update(cx, |this, cx| this.observe(position, cx))?;
    let deliver = gesture::deliver(gesture, dock_window, owner.downgrade(), cx);
    owner.update(cx, |this, _| this.native.arm_keyboard(deliver))?;
    owner.update(cx, Presence::sync_escape)?;
    schedule_diagnostic_launch(&owner, flags, cx);
    cx.set_global(PresenceApp { _owner: owner });
    Ok(())
}

/// `--settle-after-ms` starts the settled interval then; `--smoke` shows the panel after a
/// second; `--smoke` and `--quit-after-ms` then quit.
fn schedule_diagnostic_launch(owner: &Entity<Presence>, flags: &Launch, cx: &mut App) {
    if let Some(delay) = diagnostics::settle_delay(cx) {
        cx.spawn(async move |cx| {
            cx.background_executor().timer(delay).await;
            cx.update(diagnostics::settle);
        })
        .detach();
    }
    if std::env::args().any(|arg| arg == "--smoke") {
        let weak = owner.downgrade();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            if let Ok(Err(error)) =
                weak.update(cx, |this, cx| this.set_panel(PanelVisibility::Visible, cx))
            {
                tracing::error!(%error, "can't show the smoke panel");
            }
        })
        .detach();
    }
    if let Some(delay) = flags.quit_delay {
        let weak = owner.downgrade();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(delay).await;
            let _closed = weak.update(cx, Presence::quit);
        })
        .detach();
    }
}

/// The peek's height in logical pixels: Kumo's row, one row of actions and the insets.
const PEEK_HEIGHT: f32 = 124.0;

fn listen(
    states: Receiver<LinkEvent>,
    event_changes: Receiver<PresenceEvent>,
    cx: &mut Context<'_, Presence>,
) {
    cx.spawn(async move |this, cx| {
        while let Ok(event) = event_changes.recv().await {
            let result = this.update(cx, |this: &mut Presence, cx| {
                match event {
                    // Hover opens the peek; a press does nothing until C2a's pull starts there.
                    PresenceEvent::DockPressed { .. } => {}
                    PresenceEvent::PointerChanged { entered_qpc } => {
                        this.pointer_moved(entered_qpc, cx)?;
                    }
                    PresenceEvent::Position(index) => {
                        if let Some(position) = Position::ALL.get(index) {
                            this.position(*position, cx)?;
                        }
                    }
                    PresenceEvent::Retry => {
                        if this.state == LinkState::Failed {
                            this.link.retry();
                        }
                    }
                    PresenceEvent::DismissPanel if this.threads_open() => {
                        this.dismiss_threads(cx)?;
                    }
                    PresenceEvent::DismissPanel if this.text_open() => this.dismiss_text(cx)?,
                    PresenceEvent::DismissPanel => this.set_panel(PanelVisibility::Hidden, cx)?,
                    PresenceEvent::Escape => this.escape(cx)?,
                    PresenceEvent::Quit => this.quit(cx),
                    PresenceEvent::Repaint => this.dock.update(cx, |_, cx| cx.notify()),
                    event @ (PresenceEvent::CaptureLost { .. } | PresenceEvent::Reconcile) => {
                        kumo_view::bridge(&this.kumo, event, cx);
                    }
                }
                Ok::<_, OpenError>(())
            });
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::error!(%error, "presence update failed"),
                Err(_) => break,
            }
        }
    })
    .detach();
    cx.spawn(async move |this, cx| {
        while let Ok(first) = states.recv().await {
            let events = crate::link::batch(first, &states);
            match this.update(cx, |this: &mut Presence, cx| this.link_events(events, cx)) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::error!(%error, "presence update failed"),
                Err(_) => break,
            }
        }
    })
    .detach();
}
