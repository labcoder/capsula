//! The resting dock line, transient status panel, typing panel and native tray share one
//! UI-thread owner, which also applies keyboard gestures and Esc.

#[cfg(debug_assertions)]
mod accounts_fixture;
mod carry;
mod deletes;
pub(crate) mod diagnostics;
pub(crate) use kumochi_shell_core::presence::drag;
mod drawings;
mod escape;
#[expect(
    clippy::as_conversions,
    reason = "eight fixed semantic slot discriminants index the fixed receipt array"
)]
mod exclusion;
mod gesture;
mod ink;
mod issue_review;
#[cfg(test)]
mod kumo_measure_tests;
mod kumo_view;
mod launch;
pub(crate) mod line;
mod link;
mod marks;
mod messages;
pub(crate) mod observer;
mod onboarding;
mod overlay;
#[cfg(test)]
mod overlay_fixture;
mod overlay_input;
mod overlay_visibility;
mod paint;
mod paint_cache;
mod paint_geometry;
mod paint_paths;
mod paint_phase;
mod paint_stroke;
#[cfg(test)]
mod paint_work_fixture;
mod panels;
mod position;
mod pull_dock;
mod pull_picture;
#[cfg(test)]
mod run_fence_tests;
#[cfg(test)]
mod screen_hold_tests;
mod settings;
mod sign_ins;
mod stage;
#[cfg(test)]
mod stop_press_tests;
#[cfg(test)]
mod test_fixture;
#[cfg(test)]
mod tests;
mod text;
mod thread_view;
mod threads;
mod voice;
#[cfg(test)]
mod voice_wiring_tests;

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
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, Global,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, point, px, size,
};
use kumochi_protocol::constants;
use kumochi_shell_core::presence::{frame_rate, geometry, kumo, perch, pulling, thread_selection};

use crate::escape::{Activity, Escape, EscapeHold, Panel as EscapePanel, TalkEscape};
use crate::input::{Effect, Gesture};
use crate::link::{LinkEvent, LinkHandle, LinkState, Session};
use crate::platform::input::{Input, input};
use crate::platform::{
    self, EscapeHotkey, Foreground, PresenceEvent, PresenceLabels, PresenceNative, PresenceSurface,
};
use crate::runs::Runs;
use crate::settings::Settings;
use crate::strings::{Strings, Text};
use crate::threads::Threads;
#[cfg(debug_assertions)]
pub(crate) use accounts_fixture::run as run_accounts_fixture;
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
    _display_watch: Option<gpui_kit::Subscription>,
    exclusion: exclusion::Exclusion<exclusion::Receipt>,
    exclusion_timer: Option<Task<()>>,
    /// The lease a sent screen hold named, until the brain says its capture ended.
    screen_lease: Option<kumochi_protocol::message::LeaseGeneration>,
    #[cfg(test)]
    exclusion_fixture: Option<Rc<std::cell::RefCell<exclusion::Fixture>>>,
    /// The running screen hold's pointer line, which the overlay paints.
    ink: ink::Held,
    /// What a screen release's stamp reads in place of the performance counter.
    #[cfg(test)]
    counter_fixture: Option<Rc<dyn Fn() -> i64>>,
    input_windows: platform::OverlayWindows,
    native: PresenceNative,
    dock_window: AnyWindowHandle,
    panel_window: AnyWindowHandle,
    dock: Entity<Dock>,
    panel: Entity<Panel>,
    /// Created on the first double tap, then hidden and reused so its draft survives.
    text: Option<TextPanel>,
    thread_panel: Option<ThreadsPanel>,
    settings_panel: Option<SettingsPanel>,
    /// The first-run window, while it shows.
    onboarding: Option<onboarding::OnboardingPanel>,
    /// Whether onboarding may still open this run.
    onboarding_run: crate::onboarding::ThisRun,
    /// The `ChatGPT` and Claude Code sign-ins the onboarding window asked for, until each ends;
    /// an owed cancel outlives the window.
    onboarding_chatgpt: crate::sign_in_mark::SignInMark,
    onboarding_claude: crate::sign_in_mark::SignInMark,
    /// The same for Settings: its own sign-ins, cancelled when it closes.
    settings_chatgpt: crate::sign_in_mark::SignInMark,
    settings_claude: crate::sign_in_mark::SignInMark,
    settings: Entity<Settings>,
    threads: Entity<Threads>,
    chat: Entity<Threads>,
    chat_notice: crate::chat_notice::ChatNotice,
    events: Sender<PresenceEvent>,
    /// The channel's receiving end, for a drop's drains.
    events_in: Receiver<PresenceEvent>,
    /// The carry after the snap and its drop target.
    carry: carry::Carry,
    link: LinkHandle,
    /// The connection the UI last applied `Connected` for, until its next state.
    session: Option<Session>,
    issue_request: u64,
    issue_review: Option<issue_review::Route>,
    state: LinkState,
    voice: voice::Voice,
    /// What `runs` and the typing panel last made Esc see.
    activity: Activity,
    runs: Runs,
    presenter: crate::presenter::Presenter,
    paint_scratch: Rc<std::cell::RefCell<paint_stroke::Scratch>>,
    paint_notice: Rc<overlay::PaintNotice>,
    scene_timer: Option<Task<()>>,
    scene_deadline: Option<Instant>,
    /// Speaker stops a busy owner hasn't taken yet, oldest first, each with its tries left.
    pending_stops: std::collections::VecDeque<(Session, kumochi_protocol::message::RunId, u8)>,
    /// The next offer of the front pending stop, or the end of its session.
    stop_retry: Option<Task<()>>,
    escape_hold: EscapeHold<EscapeHotkey>,
    /// Esc with the talk keys down, held only while a hold's keys are.
    talk_escape: TalkEscape<EscapeHotkey>,
    /// `--esc-fixture`: hold global Esc from startup until the first Esc, with no fake work.
    escape_fixture: bool,
    layout: Layout,
    /// The stage drawings were last given, and the geometry it was chosen from.
    drawing_placement: Option<stage::Placed>,
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
    /// The drawing's frames, from the dock's window.
    frames: FrameLoop,
    /// Kumo's frames, from the peek's window.
    kumo_frames: FrameLoop,
    /// The drawing overlay's window, opened when a drawing first has something to paint.
    overlay: overlay_visibility::Visibility<overlay::Overlay>,
    /// Retries deletes the control queue had no room for.
    delete_retry: Option<Task<()>>,
    /// The brain's last error, until a turn is sent or its field's retry succeeds.
    error: Option<SharedString>,
    /// A local submission refusal, shown only in the typing panel.
    refusal: Option<Text>,
}

impl Presence {
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
        let bounds = self.layout.panel;
        let place = |window: &mut Window, cx: &mut App| {
            if visibility == PanelVisibility::Visible {
                platform::place_presence_window(window, bounds)?;
            } else {
                platform::hide_presence_window(window)?;
            }
            window.bounds_changed(cx);
            Ok::<_, platform::PresenceError>(())
        };
        self.panel_window
            .update(cx, |_, window, cx| place(window, cx))
            .map_err(|error| OpenError::Gpui(error.into()))??;
        self.presenter.input.configure(&mut self.input_windows)?;
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
        let owner = self.panel_owner(cx);
        let visibility = if owner != panels::Owner::Pointer {
            panels::pointer(self.panel_visibility, owner)
        } else if self.line.contains(platform::cursor_position()?) {
            panels::over_line(owner)
        } else {
            panels::pointer(self.panel_visibility, owner)
        };
        if visibility != self.panel_visibility {
            self.set_panel_from(visibility, entered_qpc, cx)?;
        }
        if owner != panels::Owner::Pointer {
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

    /// Follows confirmed microphone collection and the settings notice in the status panel.
    // shortcut: the line carries no notice, so the offline and settings notices show only in the status panel; B2b's peek shows them by the line.
    fn refresh_dock(&self, cx: &mut Context<'_, Self>) {
        let listening = matches!(
            self.voice.status,
            Some(Text::HoldListening | Text::HoldNearLimit)
        );
        let was_listening = self.dock.update(cx, |dock, cx| {
            cx.notify();
            std::mem::replace(&mut dock.listening, listening)
        });
        if was_listening != listening {
            let motion = kumo_view::motion(cx);
            let mood = if listening {
                Mood::Listening
            } else {
                Mood::Idle
            };
            self.kumo
                .update(cx, |kumo, cx| kumo.set_mood(mood, motion, cx));
        }
        let notice = self
            .settings
            .read(cx)
            .dock_notice()
            .filter(|notice| {
                *notice == Text::SettingsNotSaved || self.state == LinkState::Connected
            })
            .or(self.settings.read(cx).native_taskbar_notice);
        let voice = self.voice.lost.or(self.voice.status);
        self.panel.update(cx, |panel, cx| {
            if panel.notice != notice || panel.voice != voice {
                panel.notice = notice;
                panel.voice = voice;
                cx.notify();
            }
        });
        self.refresh_screen(cx);
    }

    /// Applies gesture effects in order.
    fn apply_effects(
        &mut self,
        effects: [Option<Effect>; 3],
        _dock: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        for effect in effects.into_iter().flatten() {
            match effect {
                Effect::Resting => {
                    self.refresh_dock(cx);
                    diagnostics::record(cx, format_args!("resting"));
                }
                Effect::Listening | Effect::HoldStarted | Effect::HoldEnded => {}
                Effect::OpenInput => {
                    if self.onboarding.is_some() {
                        if let Err(error) = self.onboarding_double_tap(cx) {
                            tracing::error!(%error, "can't finish onboarding");
                        }
                    } else if let Err(error) = self.focus_text(cx) {
                        tracing::error!(%error, "can't open the typing panel");
                    }
                }
            }
        }
    }

    fn activity(
        runs: &Runs,
        panel: EscapePanel,
        presenter: &crate::presenter::Presenter,
    ) -> Activity {
        Self::activity_at(runs, panel, presenter, Instant::now())
    }

    fn activity_at(
        runs: &Runs,
        panel: EscapePanel,
        presenter: &crate::presenter::Presenter,
        now: Instant,
    ) -> Activity {
        Activity {
            working: runs.working() || presenter.live_work(now),
            speaking: false,
            drawing: (runs.drawing() && matches!(panel, EscapePanel::Open)) || presenter.drawn(),
            pulling: false,
        }
    }

    /// Follows a change in the runs or the typing panel: holds or releases Esc if what Esc sees
    /// changed, and shows the newest run.
    fn runs_changed(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        self.refresh_conversation(cx);
        self.runs_changed_at(Instant::now(), cx)
    }

    fn runs_changed_at(
        &mut self,
        now: Instant,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let panel = if self.text_open() {
            EscapePanel::Open
        } else {
            EscapePanel::Closed
        };
        let mut activity = Activity {
            pulling: self.kumo.read(cx).pull_live(),
            ..Self::activity_at(&self.runs, panel, &self.presenter, now)
        };
        activity.working |= self.voice_working();
        if activity == self.activity {
            return Ok(());
        }
        self.activity = activity;
        if let Err(error) = self.sync_escape(cx) {
            self.end_scene_connection(cx);
            self.runs.disconnected();
            self.draw_changed_at(now, cx);
            self.activity = Activity {
                pulling: activity.pulling,
                ..Self::activity_at(&self.runs, panel, &self.presenter, now)
            };
            self.sync_escape_state(cx);
            return Err(error);
        }
        Ok(())
    }

    /// Holds or releases global Esc to match what's moving, showing Stop at once if Windows
    /// refuses, in the status panel and in an open typing panel.
    fn sync_escape(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        let changed = self.sync_escape_state(cx);
        if changed
            || panels::pointer(self.panel_visibility, self.panel_owner(cx)) != self.panel_visibility
        {
            self.pointer_moved(None, cx)?;
        }
        Ok(())
    }

    fn sync_escape_state(&mut self, cx: &mut Context<'_, Self>) -> bool {
        let wanted =
            self.activity.holds_global_escape() || self.voice_working() || self.escape_fixture;
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
        changed
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

    /// One event from the presence channel.
    fn on_event(
        &mut self,
        event: PresenceEvent,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        match event {
            PresenceEvent::ShowStatus | PresenceEvent::DockPressed { .. }
                if self.onboarding.is_some() =>
            {
                self.show_onboarding(onboarding::Select::Keep, cx)?;
            }
            PresenceEvent::ShowStatus => self.set_panel(PanelVisibility::Visible, cx)?,
            // Hover opens the peek; a press does nothing until C2a's pull starts there.
            PresenceEvent::DockPressed { .. } => {}
            PresenceEvent::HostChanged { .. } => self.carry_event(&event, cx),
            PresenceEvent::PointerChanged { entered_qpc } => {
                self.pointer_moved(entered_qpc, cx)?;
            }
            PresenceEvent::Position(index) => {
                if let Some(position) = kumochi_protocol::message::DockPosition::ALL.get(index) {
                    self.pick_dock_position(*position, cx)?;
                }
            }
            PresenceEvent::Retry => {
                if self.state == LinkState::Failed {
                    self.cancel_voice(cx);
                    self.link.retry();
                }
            }
            PresenceEvent::DismissPanel if self.threads_open() => {
                self.dismiss_threads(cx)?;
            }
            PresenceEvent::DismissPanel if self.text_open() => self.dismiss_text(cx)?,
            PresenceEvent::DismissPanel => self.set_panel(PanelVisibility::Hidden, cx)?,
            PresenceEvent::Escape => self.escape(cx)?,
            PresenceEvent::Quit => self.quit(cx),
            PresenceEvent::Repaint => self.dock.update(cx, |_, cx| cx.notify()),
            // The wake says nothing itself: the presenter reads its watch's bit.
            PresenceEvent::ScreenWoke => {
                let effects = self.presenter.source_woke(Instant::now());
                self.scene_effects(effects, cx);
            }
            event @ (PresenceEvent::CaptureLost { .. } | PresenceEvent::Reconcile) => {
                let effects = match event {
                    PresenceEvent::CaptureLost { seq, released } => self
                        .presenter
                        .input
                        .selection_event(drag::Event::CaptureLost { seq, released }),
                    _ => self.presenter.input.reconcile(),
                };
                let selection = self.selection_ending(effects, cx);
                if self.presenter.input.selection().is_some()
                    && matches!(event, PresenceEvent::CaptureLost { .. })
                    && input(Input::reconcile_after_release)
                {
                    let events = self.events.clone();
                    cx.defer(move |_| send(&events, PresenceEvent::Reconcile));
                }
                // Kumo's drag must hear every capture loss, even when restoring failed.
                kumo_view::bridge(&self.kumo, event, cx);
                selection?;
            }
        }
        Ok(())
    }

    fn quit(&mut self, cx: &mut Context<'_, Self>) {
        if !self.link.begin_quit() {
            return;
        }
        self.stop_voice_for_quit();
        self.revoke_exclusion(cx);
        self.end_pull(cx);
        self.end_selection(
            crate::presenter::overlay_input::ExitCause::WindowDestroyed,
            cx,
        );
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

impl Drop for Presence {
    fn drop(&mut self) {
        self.link.cancel_input();
        self.runs.cancel_hold();
        if let Some(message) = self.runs.take_hold_cancel()
            && let Some(session) = &self.session
            && !self.link.send_control(session, message)
        {
            self.link.end(session);
        }
        if let Some(session) = &self.session {
            self.link.end(session);
        }
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

type ScreenText = (Text, Option<kumochi_protocol::message::ModelName>);

struct Panel {
    kumo: Entity<KumoView>,
    state: LinkState,
    voice: Option<Text>,
    /// A settings notice the dock line can't show.
    notice: Option<Text>,
    /// The current screen hold's line, shown in place of the settings notice.
    screen_line: Option<ScreenText>,
    /// Shown only when Esc is wanted globally but Windows refused it.
    stop: bool,
    events: Sender<PresenceEvent>,
    threads: Option<WeakEntity<Presence>>,
}

impl Render for Panel {
    #[expect(
        clippy::too_many_lines,
        reason = "one status panel keeps its controls and accessible status together"
    )]
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let status = strings.get(self.voice.unwrap_or_else(|| status_text(self.state)));
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
            let button = Button::new("stop")
                .small()
                .label(strings.get(Text::Stop))
                .on_click(move |_, _, _| {
                    send(&events, PresenceEvent::Escape);
                })
                .debug_selector(|| "presence-stop".into());
            Target(button.into_element())
        });
        let line = self
            .screen_line
            .as_ref()
            .map(|(text, model)| voice::screen_text(strings, *text, model.as_ref()))
            .or_else(|| self.notice.map(|notice| strings.get(notice)));
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
                    .child(
                        div().flex().items_center().gap_1p5().text_sm().child(
                            div()
                                .id("presence-status")
                                .role(gpui_kit::Role::Status)
                                .aria_label(status.clone())
                                .child(status),
                        ),
                    )
                    .children(line.map(|line| {
                        div()
                            .text_sm()
                            .debug_selector({
                                let line = line.clone();
                                move || format!("peek-line {line}")
                            })
                            .child(line)
                    })),
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
                .primary()
                .debug_selector(|| "presence-chat".into()),
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
        LinkState::OutputUnavailable => Text::OutputUnavailable,
    }
}

/// A native surface or GPUI window couldn't be initialized or placed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OpenError {
    #[error(transparent)]
    Exclusion(#[from] exclusion::Error),
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
    #[error(
        "--draw-timing requires --gate-observer, cannot accompany --receipt-fixture, and comes at most once"
    )]
    DrawTiming,
    #[error("--accounts-fixture needs a debug build, one fixed state and a quit deadline")]
    AccountsFixture,
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
    input_windows: &mut platform::OverlayWindows,
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
            voice: None,
            notice: None,
            screen_line: None,
            stop: false,
            events: panel_events,
            threads: None,
        })
    })
    .map_err(|error| OpenError::Gpui(error.into()))?;
    let status_events = events.clone();
    panel_window
        .update(cx, |_, window, cx| {
            configure_surface(window, PresenceSurface::Panel, status_events, cx)?;
            input_windows.bind(crate::presenter::overlay_input::Layer::Panels, window)
        })
        .map_err(|error| OpenError::Gpui(error.into()))??;
    Ok((panel_window, panel))
}

/// Opens a small nonactivating dock and arms typing and admitted voice holds.
/// The caller starts the brain link afterwards.
#[expect(
    clippy::too_many_lines,
    reason = "constructs one presence owner and wires its windows in lifecycle order"
)]
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
    let (dock_window, dock) = gpui_kit::open_window(options(), cx, |window, cx| {
        window.set_window_title(constants::DISPLAY_NAME);
        window.on_window_should_close(cx, |_, _| false);
        cx.new(|_| Dock::new())
    })
    .map_err(|error| OpenError::Gpui(error.into()))?;
    let mut input_windows = platform::OverlayWindows::default();
    let input = crate::presenter::overlay_input::OverlayOwner::default();
    let scale = dock_window
        .update(cx, |_, window, cx| {
            overlay_input::configure_dock(window, events.clone(), &input, &mut input_windows, cx)?;
            Ok::<_, platform::PresenceError>(window.scale_factor())
        })
        .map_err(|error| OpenError::Gpui(error.into()))??;
    let mask = pull_picture::largest_area(line::slot(scale), scale);
    let native = PresenceNative::new(presence_labels(cx), events.clone(), mask)?;
    input_windows.proxy(native.input_proxy());
    let kumo = cx.new(|_| KumoView::new());
    let (panel_window, panel) = status_window(&events, kumo.clone(), &mut input_windows, cx)?;
    let gesture = Rc::new(Cell::new(Gesture::default()));
    let default_canvas = kumochi_scene::layout::DEFAULT_CANVAS;
    kumochi_scene::deferred::start().map_err(OpenError::Dropper)?;
    let paint_scratch = Rc::new(std::cell::RefCell::new(paint_stroke::Scratch::new()));
    let stage = paint_cache::Stager::new(&paint_scratch);
    let mut presenter = crate::presenter::Presenter::new(
        kumochi_scene::layout::SurfaceGeometry::Overlay { default_canvas },
        std::sync::Arc::new(kumochi_scene::theme::Theme::default()),
        Box::new(paint_cache::Stager(
            stage.0 + size_of::<overlay::PaintNotice>() + 2 * size_of::<usize>(),
        )),
    );
    presenter.input = input;
    if cx.has_global::<Observer>() {
        cx.global::<Observer>().observe_draw_timing(&mut presenter);
    }
    #[cfg(windows)]
    presenter.watch_sources(Box::new(platform::ScreenSources::new(events.clone())));
    let (threads, chat) = Threads::pair();
    let owner = cx.new(|cx| {
        listen(states, event_changes.clone(), cx);
        Presence {
            _display_watch: Some(stage::watch_displays(cx.weak_entity(), cx)),
            exclusion: exclusion::Exclusion::default(),
            exclusion_timer: None,
            screen_lease: None,
            #[cfg(test)]
            exclusion_fixture: None,
            ink: Rc::default(),
            #[cfg(test)]
            counter_fixture: None,
            input_windows,
            native,
            dock_window,
            panel_window,
            dock,
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
            events_in: event_changes,
            carry: carry::Carry::default(),
            link,
            session: None,
            issue_request: 0,
            issue_review: None,
            state: LinkState::Connecting,
            voice: voice::Voice::default(),
            activity: Activity::default(),
            runs: Runs::default(),
            presenter,
            paint_scratch,
            paint_notice: Rc::default(),
            scene_timer: None,
            scene_deadline: None,
            pending_stops: std::collections::VecDeque::new(),
            stop_retry: None,
            escape_hold: EscapeHold::default(),
            talk_escape: TalkEscape::default(),
            escape_fixture: flags.escape_fixture,
            layout: Layout::default(),
            drawing_placement: None,
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
            kumo_frames: FrameLoop::default(),
            overlay: overlay_visibility::Visibility::Closed,
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
    let (link, target, paused, gates) = {
        let this = owner.read(cx);
        (
            this.link.clone(),
            Rc::clone(&this.voice.target),
            Rc::clone(&this.voice.paused),
            this.voice.gates.clone(),
        )
    };
    owner.update(cx, Presence::dispatcher);
    let deliver = gesture::deliver(
        gesture,
        dock_window,
        owner.downgrade(),
        link,
        target,
        paused,
        gates,
        cx,
    );
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
            let result = this.update(cx, |this: &mut Presence, cx| this.on_event(event, cx));
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
