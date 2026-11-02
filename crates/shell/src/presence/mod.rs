//! The resting dock, its stretch, transient status panel, typing panel and native tray share one
//! UI-thread owner, which also applies keyboard gestures and Esc.

pub(crate) mod diagnostics;
mod geometry;
mod gesture;
mod launch;
mod link;
pub(crate) mod observer;
mod outline;
mod paint;
mod panels;
mod stretch;
mod text;
mod thread_view;
mod threads;

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use capsula_protocol::constants;
use gpui_kit::component::input::InputState;
use gpui_kit::component::{ActiveTheme as _, button::Button};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, Global, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Task, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, canvas, div, point, px,
    size,
};

use crate::escape::{self, Activity, Escape, EscapeHold, Panel as EscapePanel};
use crate::input::{Effect, Gesture};
use crate::link::{LinkEvent, LinkHandle, LinkState, Session};
use crate::platform::{
    self, Capture, EscapeHotkey, EventTime, Foreground, PresenceEvent, PresenceLabels,
    PresenceNative, PresenceSurface,
};
use crate::runs::{Refused, Runs};
use crate::strings::{Strings, Text};
use crate::threads::Threads;
pub(crate) use diagnostics::Diagnostics;
use diagnostics::PendingPaint;
use geometry::{Layout, PanelVisibility, Position};
pub(crate) use launch::Launch;
pub(crate) use observer::Observer;
use observer::{Drawn, Surface};
use outline::Outline;
use paint::{OutlinePaint, PanelStamp, Target};
use stretch::Stretch;
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
    escape_hold: EscapeHold<EscapeHotkey>,
    /// `--esc-fixture`: hold global Esc from startup until the first Esc, with no fake work.
    escape_fixture: bool,
    layout: Layout,
    panel_visibility: PanelVisibility,
    scale: f32,
    pointer_settle: Option<Task<()>>,
    stretch: Stretch,
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
        self.layout = position.layout(
            area,
            [physical(112.0), physical(48.0)],
            [physical(300.0), physical(140.0)],
            self.stretch.extent(),
        );
        self.dock_window
            .update(cx, |_, window, cx| {
                platform::place_presence_window(window, self.layout.overlay)?;
                window.bounds_changed(cx);
                Ok::<_, platform::PresenceError>(())
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        let index = Position::ALL
            .iter()
            .position(|candidate| *candidate == position)
            .expect("every dock position has a menu entry");
        self.stretch.reposition();
        let proposal = outline::Ownership::only(self.outline());
        if !self.reshape(proposal, cx) {
            // Frames retry the upload until the mask takes it.
            self.start_ticking(cx);
        }
        self.native.show(index)?;
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
                    platform::place_hidden_presence_window(window, self.layout.panel)?;
                }
                window.bounds_changed(cx);
                Ok::<_, platform::PresenceError>(())
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        self.panel_visibility = visibility;
        Ok(())
    }

    fn pointer_changed(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        let visibility = panels::pointer(self.panel_visibility, self.panel_owner());
        if visibility != self.panel_visibility {
            self.set_panel(visibility, cx)?;
        }
        if self.text_open() || self.threads_open() || self.escape_hold.shows_stop() {
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
                    let visibility = panels::left(this.panel_owner());
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

    /// Whether the pointer is over the presence's outline, where the hit proxy takes it, or
    /// over the panel when it's shown.
    fn hovered(&self, point: [i32; 2], panel: PanelVisibility) -> bool {
        self.owned().contains(point) || self.layout.panel_contains(point, panel)
    }

    fn refresh_dock(&self, cx: &mut Context<'_, Self>) {
        let label: SharedString = if self.gesture.get().listening() {
            cx.global::<Strings>().get(Text::Listening).into()
        } else {
            dock_label(self.state, cx)
        };
        self.dock.update(cx, |dock, cx| {
            dock.label = label;
            cx.notify();
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

    /// Hides every Capsula surface from capture before the hold is announced, and shows them
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
        for window in [Some(self.panel_window), text, threads]
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

    /// One Esc press, from the hotkey, the typing panel or Stop. The local effect happens here,
    /// and only then is `Stop` or `ClearDrawing` queued for the brain, which nothing waits on.
    fn escape(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        let panel = if self.threads_focused() {
            EscapePanel::Threads
        } else if self.text_open() {
            EscapePanel::Open
        } else {
            EscapePanel::Closed
        };
        let action = escape::decide(self.activity, panel);
        let tell = match action {
            Escape::Stop => self.runs.stop(),
            Escape::Clear => {
                let effect = self.runs.clear();
                self.grant(effect.credit);
                effect.clear
            }
            Escape::Dismiss => {
                if panel == EscapePanel::Threads {
                    self.dismiss_threads(cx)?;
                } else {
                    self.dismiss_text(cx)?;
                }
                None
            }
            Escape::Ignore => None,
        };
        self.activity = self.activity();
        self.refresh_conversation(cx);
        // Stamped once the local stop or clear is applied, for the Esc smoke's timing.
        diagnostics::record(cx, format_args!("escape action={action:?}"));
        if let Some(message) = tell {
            self.tell(message);
        }
        self.escape_fixture = false;
        self.sync_escape(cx)
    }

    /// What's moving or on screen, as Esc sees it. shortcut: the placeholder steps show only in
    /// the typing panel, so they count as a drawing only while it's open; slice 4 draws on the
    /// overlay.
    fn activity(&self) -> Activity {
        Activity {
            working: self.runs.working(),
            speaking: false,
            drawing: self.runs.drawing() && self.text_open(),
        }
    }

    /// Follows a change in the runs or the typing panel: holds or releases Esc if what Esc sees
    /// changed, and shows the newest run.
    fn runs_changed(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        self.refresh_conversation(cx);
        let activity = self.activity();
        if activity == self.activity {
            return Ok(());
        }
        self.activity = activity;
        self.sync_escape(cx)
    }

    /// Enter in the typing panel: sends the draft as a turn and empties the box, or keeps the
    /// draft and says why it wasn't sent.
    pub(super) fn submit(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        let Some(text) = &self.text else {
            return Ok(());
        };
        let draft = text.input.read(cx).value();
        let (link, session) = (&self.link, &self.session);
        let result = self.runs.submit(&draft, |turn| {
            session
                .as_ref()
                .is_some_and(|session| link.send_data(session, turn))
        });
        diagnostics::record(cx, format_args!("submit sent={}", result.is_ok()));
        let notice = match result {
            Ok(()) => {
                let input = text.input.clone();
                text.window
                    .update(cx, |_, window, cx| {
                        input.update(cx, |state, cx| state.set_value("", window, cx));
                    })
                    .map_err(|error| OpenError::Gpui(error.into()))?;
                None
            }
            Err(Refused::Blank) => None,
            Err(Refused::Busy) => Some(Text::TurnBusy),
            Err(Refused::Offline) => Some(Text::TurnOffline),
            Err(Refused::TooLong) => Some(Text::TurnTooLong),
        };
        let notice: Option<SharedString> =
            notice.map(|notice| cx.global::<Strings>().get(notice).into());
        text.view.update(cx, |view, cx| {
            view.notice = notice;
            cx.notify();
        });
        self.runs_changed(cx)
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
                view.stop = stop;
                cx.notify();
            });
        }
        if let Some(panel) = &self.thread_panel {
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

    fn quit(&self, cx: &mut Context<'_, Self>) {
        if !self.link.begin_quit() {
            return;
        }
        let link = self.link.clone();
        diagnostics::record(cx, format_args!("stretch totals {}", self.stretch.totals()));
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

struct Dock {
    label: SharedString,
    /// Used only with `--input-diagnostics`; the next render takes it into a paint-phase marker.
    pending_paint: PendingPaint,
    /// The body to paint, in physical screen pixels.
    outline: Outline,
    /// The dock window's top-left on screen, in physical pixels.
    origin: [i32; 2],
    /// The accepted mask state `outline` came from, for `--gate-observer`.
    drawn: Drawn,
}

impl Render for Dock {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        // Recorded in GPUI's paint phase, not at present: PresentMon must join it to the screen.
        let marker = self.pending_paint.take().map(|at| {
            canvas(
                |_, _, _| (),
                move |_, (), _, cx| {
                    let painted_us = at.elapsed().as_micros();
                    diagnostics::record(cx, format_args!("listening painted_us={painted_us}"));
                },
            )
            .absolute()
            .size_full()
        });
        let scale = window.scale_factor();
        let [x, y, width, height] = self.outline.pill;
        let [origin_x, origin_y] = self.origin.map(f64::from);
        div()
            .size_full()
            .relative()
            .child(OutlinePaint {
                outline: self.outline,
                origin: self.origin,
                drawn: self.drawn,
                fill: cx.theme().background,
                border: cx.theme().border,
            })
            .child(
                div()
                    .absolute()
                    .left(logical(x - origin_x, scale))
                    .top(logical(y - origin_y, scale))
                    .w(logical(width, scale))
                    .h(logical(height, scale))
                    .text_color(cx.theme().foreground)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .child(self.label.clone()),
            )
            // After the frame's stamp, so the paint qpc it logs falls inside that frame.
            .children(marker)
    }
}

/// Physical pixels as GPUI's logical pixels.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "screen coordinates fit f32"
)]
fn logical(physical: f64, scale: f32) -> gpui_kit::Pixels {
    px(physical as f32 / scale)
}

struct Panel {
    state: LinkState,
    /// Shown only when Esc is wanted globally but Windows refused it.
    stop: bool,
    events: Sender<PresenceEvent>,
    threads: Option<WeakEntity<Presence>>,
}

impl Render for Panel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let quit_events = self.events.clone();
        let threads_owner = self.threads.clone();
        let retry = (self.state == LinkState::Failed).then(|| {
            let events = self.events.clone();
            Button::new("retry")
                .label(strings.get(Text::Retry))
                .on_click(move |_, _, _| {
                    send(&events, PresenceEvent::Retry);
                })
        });
        let stop = self.stop.then(|| {
            let events = self.events.clone();
            Target(
                Button::new("stop")
                    .label(strings.get(Text::Stop))
                    .on_click(move |_, _, _| {
                        send(&events, PresenceEvent::Escape);
                    })
                    .into_element(),
            )
        });
        div()
            .size_full()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .flex()
            .flex_col()
            .gap_2()
            .children(cx.has_global::<Observer>().then_some(PanelStamp))
            .child(constants::DISPLAY_NAME)
            .child(strings.get(status_text(self.state)))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .children(stop)
                    .children(retry)
                    .child(
                        Button::new("threads")
                            .label(strings.get(Text::OpenThreads))
                            .on_click(move |_, _, cx| {
                                if let Some(owner) = threads_owner.clone() {
                                    cx.defer(move |cx| {
                                        if let Ok(Err(error)) =
                                            owner.update(cx, Presence::open_threads)
                                        {
                                            tracing::error!(%error, "can't open the threads panel");
                                        }
                                    });
                                }
                            }),
                    )
                    .child(Button::new("quit").label(strings.get(Text::Quit)).on_click(
                        move |_, _, _| {
                            send(&quit_events, PresenceEvent::Quit);
                        },
                    )),
            )
    }
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

fn dock_label(state: LinkState, cx: &App) -> SharedString {
    cx.global::<Strings>()
        .get(if state == LinkState::Failed {
            Text::DockOffline
        } else {
            status_text(state)
        })
        .into()
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
    #[error("--stretch-fixture needs --quit-after-ms or --smoke, so its frames always stop")]
    StretchFixtureDeadline,
    #[error("--gate-observer and --receipt-fixture need --quit-after-ms or --smoke")]
    ObserverDeadline,
    #[error("--receipt-fixture needs --gate-observer and comes at most once")]
    ReceiptFixture,
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
    let initial_label = dock_label(LinkState::Connecting, cx);
    let (dock_window, dock) = gpui_kit::open_window(options(), cx, |window, cx| {
        window.set_window_title(constants::DISPLAY_NAME);
        window.on_window_should_close(cx, |_, _| false);
        cx.new(|_| Dock {
            label: initial_label,
            pending_paint: PendingPaint::default(),
            outline: Outline::default(),
            origin: [0; 2],
            drawn: Drawn::default(),
        })
    })
    .map_err(|error| OpenError::Gpui(error.into()))?;
    let scale = dock_window
        .update(cx, |_, window, cx| {
            configure_surface(window, PresenceSurface::Overlay, dock_events, cx)?;
            Ok::<_, platform::PresenceError>(window.scale_factor())
        })
        .map_err(|error| OpenError::Gpui(error.into()))??;
    let stretch = Stretch::new(scale);
    let slot = [scaled(112.0, scale), scaled(48.0, scale)];
    let native = PresenceNative::new(labels, events.clone(), stretch.mask_size(slot))?;
    let panel_events = events.clone();
    let close_events = events.clone();
    let (panel_window, panel) = gpui_kit::open_window(options(), cx, |window, cx| {
        window.set_window_title(constants::DISPLAY_NAME);
        window.on_window_should_close(cx, move |_, _| {
            send(&close_events, PresenceEvent::DismissPanel);
            false
        });
        cx.new(|_| Panel {
            state: LinkState::Connecting,
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
    let gesture = Rc::new(Cell::new(Gesture::default()));
    let owner_gesture = Rc::clone(&gesture);
    let threads = cx.new(|_| Threads::default());
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
            threads,
            events,
            link,
            session: None,
            state: LinkState::Connecting,
            gesture: owner_gesture,
            activity: Activity::default(),
            runs: Runs::default(),
            escape_hold: EscapeHold::default(),
            escape_fixture: flags.escape_fixture,
            layout: Layout {
                dock: [0; 4],
                panel: [0; 4],
                overlay: [0; 4],
            },
            panel_visibility: PanelVisibility::Hidden,
            scale,
            pointer_settle: None,
            stretch,
        }
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

/// `--stretch-fixture` starts stretching now; `--smoke` shows the panel after a second;
/// `--smoke` and `--quit-after-ms` then quit.
fn schedule_diagnostic_launch(owner: &Entity<Presence>, flags: &Launch, cx: &mut App) {
    if flags.stretch_fixture {
        owner.update(cx, Presence::start_fixture);
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
            let _closed = weak.update(cx, |this, cx| this.quit(cx));
        })
        .detach();
    }
}

/// Link events applied in one update at most [guess]: a fast stream is applied in batches
/// rather than one UI update per frame.
const EVENT_BATCH: usize = 256;

fn listen(
    states: Receiver<LinkEvent>,
    event_changes: Receiver<PresenceEvent>,
    cx: &mut Context<'_, Presence>,
) {
    cx.spawn(async move |this, cx| {
        while let Ok(event) = event_changes.recv().await {
            let result = this.update(cx, |this: &mut Presence, cx| {
                match event {
                    PresenceEvent::DockPressed { qpc } => {
                        let visibility = panels::pressed(this.panel_visibility, this.panel_owner());
                        this.pointer_settle = None;
                        if visibility != this.panel_visibility {
                            this.set_panel_from(visibility, Some(qpc), cx)?;
                        }
                    }
                    PresenceEvent::PointerChanged => {
                        this.follow_pointer(cx);
                        this.pointer_changed(cx)?;
                    }
                    PresenceEvent::PointerMoved => this.follow_pointer(cx),
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
            let mut events = vec![first];
            while events.len() < EVENT_BATCH
                && let Ok(event) = states.try_recv()
            {
                events.push(event);
            }
            match this.update(cx, |this: &mut Presence, cx| this.link_events(events, cx)) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::error!(%error, "presence update failed"),
                Err(_) => break,
            }
        }
    })
    .detach();
}
