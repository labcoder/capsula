//! The resting dock, its stretch, transient hover panel, typing panel and native tray share one
//! UI-thread owner, which also applies keyboard gestures and Esc.

pub(crate) mod diagnostics;
mod geometry;
mod gesture;
mod outline;
mod paint;
mod stretch;

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use capsula_protocol::constants;
use gpui_kit::component::input::{Escape as EscapeKey, Input, InputState};
use gpui_kit::component::{ActiveTheme as _, button::Button};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, Global,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Task, Window, WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, canvas, div,
    point, px, size,
};

use crate::escape::{self, Activity, Escape, EscapeHold, Panel as EscapePanel};
use crate::input::{Effect, Gesture};
use crate::link::{LinkHandle, LinkState};
use crate::platform::{
    self, Capture, EscapeHotkey, EventTime, Foreground, PresenceEvent, PresenceLabels,
    PresenceNative, PresenceSurface,
};
use crate::strings::{Strings, Text};
pub(crate) use diagnostics::Diagnostics;
use diagnostics::PendingPaint;
use geometry::{Layout, PanelVisibility, Position};
use outline::Outline;
use paint::OutlinePaint;
use stretch::Stretch;

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
    events: Sender<PresenceEvent>,
    link: LinkHandle,
    state: LinkState,
    /// The canonical gesture; the Raw Input callback reduces it before GPUI sees anything.
    gesture: Rc<Cell<Gesture>>,
    activity: Activity,
    escape_hold: EscapeHold<EscapeHotkey>,
    /// `--esc-fixture`: hold global Esc from startup until the first Esc, with no fake work.
    escape_fixture: bool,
    layout: Layout,
    panel_visibility: PanelVisibility,
    scale: f32,
    pointer_settle: Option<Task<()>>,
    stretch: Stretch,
}

/// The typing surface: the one window Capsula activates, and only on a double tap.
struct TextPanel {
    window: AnyWindowHandle,
    view: Entity<TextView>,
    input: Entity<InputState>,
    /// The window in front before the panel took focus, given back when it closes.
    previous: Option<Foreground>,
    visible: bool,
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
        // The typing panel sits where the hover panel would, and hover never dismisses it.
        if self.text_open() {
            return Ok(());
        }
        // Stop replaces global Esc, so it stays on screen until Esc is held or not wanted.
        if self.escape_hold.shows_stop() {
            self.pointer_settle = None;
            if self.panel_visibility == PanelVisibility::Hidden {
                self.set_panel(PanelVisibility::Visible, cx)?;
            }
            return Ok(());
        }
        let point = platform::cursor_position()?;
        if self.hovered(point, self.panel_visibility) {
            self.pointer_settle = None;
            if self.panel_visibility == PanelVisibility::Hidden {
                self.set_panel(PanelVisibility::Visible, cx)?;
            }
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
                    this.set_panel(PanelVisibility::Hidden, cx)?;
                }
                Ok::<_, OpenError>(())
            });
            if let Ok(Err(error)) = result {
                tracing::error!(%error, "can't dismiss the hover panel");
            }
        }));
        Ok(())
    }

    fn text_open(&self) -> bool {
        self.text.as_ref().is_some_and(|text| text.visible)
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
        for window in [Some(self.panel_window), text].into_iter().flatten() {
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

    fn open_text(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        if self.text.is_none() {
            self.text = Some(self.create_text(cx)?);
        }
        self.pointer_settle = None;
        self.set_panel(PanelVisibility::Hidden, cx)?;
        let bounds = self.layout.panel;
        let Some(text) = self.text.as_mut() else {
            return Ok(());
        };
        text.window
            .update(cx, |_, window, cx| {
                platform::place_presence_window(window, bounds)?;
                window.bounds_changed(cx);
                Ok::<_, platform::PresenceError>(())
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        text.visible = true;
        let input = text.input.clone();
        let activated = text
            .window
            .update(cx, |_, window, cx| {
                let previous = platform::activate_presence_window(window)?;
                input.update(cx, |state, cx| state.focus(window, cx));
                Ok::<_, platform::PresenceError>(previous)
            })
            .map_err(|error| OpenError::Gpui(error.into()))?;
        let focused = match activated {
            Ok(Some(previous)) => {
                text.previous = Some(previous);
                true
            }
            Ok(None) => true,
            // Still visible; a click activates it.
            Err(error) => {
                tracing::warn!(%error, "can't focus the typing panel");
                false
            }
        };
        diagnostics::record(cx, format_args!("input opened focused={focused}"));
        Ok(())
    }

    fn create_text(&self, cx: &mut Context<'_, Self>) -> Result<TextPanel, OpenError> {
        let stop = self.escape_hold.shows_stop();
        let events = self.events.clone();
        let close_events = self.events.clone();
        let (window, view) = gpui_kit::open_window(options(), cx, |window, cx| {
            window.set_window_title(constants::DISPLAY_NAME);
            window.on_window_should_close(cx, move |_, _| {
                send(&close_events, PresenceEvent::DismissPanel);
                false
            });
            let input = cx.new(|cx| InputState::new(window, cx));
            cx.new(|_| TextView {
                input,
                stop,
                events,
            })
        })
        .map_err(|error| OpenError::Gpui(error.into()))?;
        let events = self.events.clone();
        window
            .update(cx, |_, window, cx| {
                configure_surface(window, PresenceSurface::Text, events, cx)
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        Ok(TextPanel {
            window,
            input: view.read(cx).input.clone(),
            view,
            previous: None,
            visible: false,
        })
    }

    /// Hides the typing panel, keeping its draft, and gives the front back if it still has it.
    fn dismiss_text(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        let bounds = self.layout.panel;
        let Some(text) = self.text.as_mut().filter(|text| text.visible) else {
            return Ok(());
        };
        text.visible = false;
        let previous = text.previous.take();
        text.window
            .update(cx, |_, window, cx| {
                if let Some(previous) = previous {
                    platform::restore_foreground(window, previous);
                }
                platform::place_hidden_presence_window(window, bounds)?;
                window.bounds_changed(cx);
                Ok::<_, platform::PresenceError>(())
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        diagnostics::record(cx, format_args!("input dismissed"));
        // The typing panel was covering the hover panel's Stop.
        if self.escape_hold.shows_stop() {
            self.pointer_changed(cx)?;
        }
        Ok(())
    }

    /// One Esc press, from the hotkey, the typing panel or Stop. The local effect happens here,
    /// before any brain message; slice 2 adds `Stop` and slice 4 `ClearDrawing` after it.
    fn escape(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        let panel = if self.text_open() {
            EscapePanel::Open
        } else {
            EscapePanel::Closed
        };
        let action = escape::decide(self.activity, panel);
        match action {
            Escape::Stop => {
                self.activity.working = false;
                self.activity.speaking = false;
            }
            Escape::Clear => self.activity.drawing = false,
            Escape::Dismiss => self.dismiss_text(cx)?,
            Escape::Ignore => {}
        }
        diagnostics::record(cx, format_args!("escape action={action:?}"));
        self.escape_fixture = false;
        self.sync_escape(cx)
    }

    /// Holds or releases global Esc to match what's moving, showing Stop at once if Windows
    /// refuses, in the hover panel and in an open typing panel.
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
        if changed {
            self.pointer_changed(cx)?;
        }
        Ok(())
    }

    fn quit(&self, cx: &mut Context<'_, Self>) {
        if !self.link.begin_quit() {
            return;
        }
        let link = self.link.clone();
        diagnostics::record(cx, format_args!("stretch totals {}", self.stretch.totals()));
        let diagnostics = diagnostics::release(cx);
        let told = cx.background_executor().spawn(async move {
            link.finish_quit();
            drop(diagnostics);
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
            .children(marker)
            .child(OutlinePaint {
                outline: self.outline,
                origin: self.origin,
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
}

impl Render for Panel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let quit_events = self.events.clone();
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
            Button::new("stop")
                .label(strings.get(Text::Stop))
                .on_click(move |_, _, _| {
                    send(&events, PresenceEvent::Escape);
                })
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
            .child(constants::DISPLAY_NAME)
            .child(strings.get(status_text(self.state)))
            .child(
                div().flex().gap_2().children(stop).children(retry).child(
                    Button::new("quit")
                        .label(strings.get(Text::Quit))
                        .on_click(move |_, _, _| {
                            send(&quit_events, PresenceEvent::Quit);
                        }),
                ),
            )
    }
}

/// The minimum anchored typing surface: one focused input and nothing that can't yet work.
struct TextView {
    input: Entity<InputState>,
    /// The Stop fallback, here too because this panel covers the hover panel's.
    stop: bool,
    events: Sender<PresenceEvent>,
}

impl Render for TextView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let events = self.events.clone();
        let stop = self.stop.then(|| {
            let events = self.events.clone();
            Button::new("stop")
                .label(cx.global::<Strings>().get(Text::Stop))
                .on_click(move |_, _, _| send(&events, PresenceEvent::Escape))
        });
        div()
            .size_full()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .flex()
            .flex_col()
            .gap_2()
            .on_action(move |_: &EscapeKey, _, _| send(&events, PresenceEvent::Escape))
            .child(Input::new(&self.input))
            .children(stop)
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

/// Opens a small nonactivating dock immediately and arms the keyboard, so Listening works
/// from the first frame. The caller starts the brain link afterwards.
pub(crate) fn open(
    states: Receiver<LinkState>,
    link: LinkHandle,
    diagnostics: Option<Diagnostics>,
    cx: &mut App,
) -> Result<(), OpenError> {
    let flags = launch(std::env::args().skip(1))?;
    if let Some(diagnostics) = diagnostics {
        cx.set_global(diagnostics);
    }
    let (events, event_changes) = async_channel::unbounded();
    let strings = cx.global::<Strings>();
    let labels = PresenceLabels {
        tooltip: constants::DISPLAY_NAME.to_owned(),
        position_menu: strings.get(Text::DockPosition),
        positions: Text::POSITIONS.map(|text| strings.get(text)),
        retry: strings.get(Text::Retry),
        quit: strings.get(Text::Quit),
    };
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
        })
    })
    .map_err(|error| OpenError::Gpui(error.into()))?;
    let hover_events = events.clone();
    panel_window
        .update(cx, |_, window, cx| {
            configure_surface(window, PresenceSurface::Panel, hover_events, cx)
        })
        .map_err(|error| OpenError::Gpui(error.into()))??;
    let gesture = Rc::new(Cell::new(Gesture::default()));
    let owner_gesture = Rc::clone(&gesture);
    let owner = cx.new(|cx| {
        listen(states, event_changes, cx);
        Presence {
            native,
            dock_window,
            panel_window,
            dock,
            panel,
            text: None,
            events,
            link,
            state: LinkState::Connecting,
            gesture: owner_gesture,
            activity: Activity::default(),
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
    owner.update(cx, |this, cx| this.position(Position::default(), cx))?;
    let deliver = gesture::deliver(gesture, dock_window, owner.downgrade(), cx);
    owner.update(cx, |this, _| this.native.arm_keyboard(deliver))?;
    owner.update(cx, Presence::sync_escape)?;
    schedule_diagnostic_launch(&owner, &flags, cx);
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

fn listen(
    states: Receiver<LinkState>,
    event_changes: Receiver<PresenceEvent>,
    cx: &mut Context<'_, Presence>,
) {
    cx.spawn(async move |this, cx| {
        while let Ok(event) = event_changes.recv().await {
            let result = this.update(cx, |this: &mut Presence, cx| {
                match event {
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
                    PresenceEvent::DismissPanel if this.text_open() => this.dismiss_text(cx)?,
                    PresenceEvent::DismissPanel => this.set_panel(PanelVisibility::Hidden, cx)?,
                    PresenceEvent::Escape => this.escape(cx)?,
                    PresenceEvent::Quit => this.quit(cx),
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
        while let Ok(state) = states.recv().await {
            if this
                .update(cx, |this: &mut Presence, cx| {
                    if state == LinkState::Failed {
                        this.native.enable_retry();
                    } else {
                        this.native.disable_retry();
                    }
                    this.state = state;
                    // Listening keeps its label until the gesture ends.
                    this.refresh_dock(cx);
                    this.panel.update(cx, |panel, cx| {
                        panel.state = state;
                        cx.notify();
                    });
                })
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
}

/// The diagnostic launch flags. Every diagnostic launch quits on its own.
#[derive(Debug, PartialEq, Eq)]
struct Launch {
    quit_delay: Option<Duration>,
    escape_fixture: bool,
    /// `--stretch-fixture`: stretch out and back on a clock, with no input, until Quit.
    stretch_fixture: bool,
}

fn launch(args: impl Iterator<Item = String>) -> Result<Launch, OpenError> {
    let mut delay = None;
    let mut escape_fixture = false;
    let mut stretch_fixture = false;
    for arg in args {
        if arg == "--smoke" {
            delay = Some(Duration::from_secs(10));
        } else if arg == "--esc-fixture" {
            escape_fixture = true;
        } else if arg == "--stretch-fixture" {
            stretch_fixture = true;
        } else if let Some(value) = arg.strip_prefix("--quit-after-ms=") {
            let millis = value.parse::<u64>().map_err(|_| OpenError::QuitDelay)?;
            if !(1..=120_000).contains(&millis) {
                return Err(OpenError::QuitDelay);
            }
            delay = Some(Duration::from_millis(millis));
        }
    }
    if escape_fixture && delay.is_none() {
        return Err(OpenError::EscFixtureDeadline);
    }
    if stretch_fixture && delay.is_none() {
        return Err(OpenError::StretchFixtureDeadline);
    }
    Ok(Launch {
        quit_delay: delay,
        escape_fixture,
        stretch_fixture,
    })
}

#[cfg(test)]
mod tests {
    use super::{Launch, OpenError, launch};
    use std::time::Duration;

    fn args(args: &[&str]) -> impl Iterator<Item = String> {
        args.iter()
            .map(|arg| (*arg).to_owned())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn diagnostic_launches_have_a_bounded_quit_deadline() {
        assert_eq!(
            launch(args(&["--smoke"])).unwrap().quit_delay,
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            launch(args(&["--quit-after-ms=75000"])).unwrap().quit_delay,
            Some(Duration::from_secs(75))
        );
        for invalid in ["0", "120001", "-1", "later"] {
            assert!(launch([format!("--quit-after-ms={invalid}")].into_iter()).is_err());
        }
    }

    #[test]
    fn the_esc_fixture_only_runs_with_a_quit_deadline() {
        assert!(matches!(
            launch(args(&["--esc-fixture"])),
            Err(OpenError::EscFixtureDeadline)
        ));
        assert_eq!(
            launch(args(&["--esc-fixture", "--quit-after-ms=500"])).unwrap(),
            Launch {
                quit_delay: Some(Duration::from_millis(500)),
                escape_fixture: true,
                stretch_fixture: false,
            }
        );
        assert!(
            launch(args(&["--smoke", "--esc-fixture"]))
                .unwrap()
                .escape_fixture
        );
        assert!(!launch(args(&[])).unwrap().escape_fixture);
    }

    #[test]
    fn the_stretch_fixture_only_runs_with_a_quit_deadline() {
        assert!(matches!(
            launch(args(&["--stretch-fixture"])),
            Err(OpenError::StretchFixtureDeadline)
        ));
        assert_eq!(
            launch(args(&["--stretch-fixture", "--quit-after-ms=70000"])).unwrap(),
            Launch {
                quit_delay: Some(Duration::from_secs(70)),
                escape_fixture: false,
                stretch_fixture: true,
            }
        );
        assert!(
            launch(args(&["--smoke", "--stretch-fixture"]))
                .unwrap()
                .stretch_fixture
        );
        assert!(!launch(args(&[])).unwrap().stretch_fixture);
    }
}
