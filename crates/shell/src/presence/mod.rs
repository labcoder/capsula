//! The resting dock, transient hover panel and native tray share one UI-thread owner.

mod geometry;

use std::time::Duration;

use async_channel::{Receiver, Sender};
use capsula_protocol::constants;
use gpui_kit::component::{ActiveTheme as _, button::Button};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, Global, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Task, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, point, px, size,
};

use crate::link::{LinkHandle, LinkState};
use crate::platform::{self, PresenceEvent, PresenceLabels, PresenceNative, PresenceSurface};
use crate::strings::{Strings, Text};
use geometry::{Layout, PanelVisibility, Position};

/// Keeps the app owner alive even when its panel is hidden.
struct PresenceApp {
    _owner: Entity<Presence>,
}
impl Global for PresenceApp {}

struct Presence {
    native: PresenceNative,
    dock_window: AnyWindowHandle,
    dock: Entity<Dock>,
    /// Opened on first show and reused, so a session that never hovers pays for no second
    /// GPUI window or swap chain.
    panel: Option<(AnyWindowHandle, Entity<Panel>)>,
    events: Sender<PresenceEvent>,
    state: LinkState,
    link: LinkHandle,
    layout: Layout,
    panel_visibility: PanelVisibility,
    scale: f32,
    pointer_settle: Option<Task<()>>,
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
        );
        self.dock_window
            .update(cx, |_, window, cx| {
                platform::place_presence_window(window, self.layout.dock)?;
                window.bounds_changed(cx);
                Ok::<_, platform::PresenceError>(())
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        let index = Position::ALL
            .iter()
            .position(|candidate| *candidate == position)
            .expect("every dock position has a menu entry");
        self.native.place(self.layout.dock, index)?;
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
        let panel_window = match (self.panel.as_ref().map(|(window, _)| *window), visibility) {
            (Some(window), _) => window,
            (None, PanelVisibility::Hidden) => {
                self.panel_visibility = visibility;
                return Ok(());
            }
            (None, PanelVisibility::Visible) => self.open_panel(cx)?,
        };
        panel_window
            .update(cx, |_, window, cx| {
                if visibility == PanelVisibility::Visible {
                    platform::place_presence_window(window, self.layout.panel)?;
                    window.bounds_changed(cx);
                } else {
                    platform::hide_presence_window(window)?;
                }
                Ok::<_, platform::PresenceError>(())
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        self.panel_visibility = visibility;
        Ok(())
    }

    fn open_panel(&mut self, cx: &mut Context<'_, Self>) -> Result<AnyWindowHandle, OpenError> {
        let panel_events = self.events.clone();
        let close_events = self.events.clone();
        let state = self.state;
        let (panel_window, panel) = gpui_kit::open_window(options(), cx, |window, cx| {
            window.set_window_title(constants::DISPLAY_NAME);
            window.on_window_should_close(cx, move |_, _| {
                send(&close_events, PresenceEvent::DismissPanel);
                false
            });
            cx.new(|_| Panel {
                state,
                events: panel_events,
            })
        })
        .map_err(|error| OpenError::Gpui(error.into()))?;
        let events = self.events.clone();
        let configured = panel_window
            .update(cx, |_, window, cx| {
                let configured = configure_surface(window, PresenceSurface::Panel, events, cx);
                if configured.is_err() {
                    window.remove_window();
                }
                configured
            })
            .map_err(|error| OpenError::Gpui(error.into()))?;
        configured?;
        self.panel = Some((panel_window, panel));
        Ok(panel_window)
    }

    fn pointer_changed(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        let point = platform::cursor_position()?;
        if self.layout.contains(point, self.panel_visibility) {
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
                if !this.layout.contains(point, PanelVisibility::Visible) {
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

    fn quit(&self, cx: &mut Context<'_, Self>) {
        if !self.link.begin_quit() {
            return;
        }
        tracing::info!(
            panel_initialized = self.panel.is_some(),
            "presence shutdown"
        );
        let link = self.link.clone();
        let told = cx
            .background_executor()
            .spawn(async move { link.finish_quit() });
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
}

impl Render for Dock {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div().size_full().p_1().child(
            div()
                .size_full()
                .rounded_full()
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().background)
                .text_color(cx.theme().foreground)
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .child(self.label.clone()),
        )
    }
}

struct Panel {
    state: LinkState,
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
                div().flex().gap_2().children(retry).child(
                    Button::new("quit")
                        .label(strings.get(Text::Quit))
                        .on_click(move |_, _, _| {
                            send(&quit_events, PresenceEvent::Quit);
                        }),
                ),
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

/// Opens a small nonactivating dock immediately, with the brain link still connecting.
pub(crate) fn open(
    states: Receiver<LinkState>,
    link: LinkHandle,
    cx: &mut App,
) -> Result<(), OpenError> {
    let quit_delay = quit_delay(std::env::args().skip(1))?;
    let (events, event_changes) = async_channel::unbounded();
    let strings = cx.global::<Strings>();
    let labels = PresenceLabels {
        tooltip: constants::DISPLAY_NAME.to_owned(),
        position_menu: strings.get(Text::DockPosition),
        positions: Text::POSITIONS.map(|text| strings.get(text)),
        retry: strings.get(Text::Retry),
        quit: strings.get(Text::Quit),
    };
    let native = PresenceNative::new(labels, events.clone())?;
    let dock_events = events.clone();
    let initial_label = dock_label(LinkState::Connecting, cx);
    let (dock_window, dock) = gpui_kit::open_window(options(), cx, |window, cx| {
        window.set_window_title(constants::DISPLAY_NAME);
        window.on_window_should_close(cx, |_, _| false);
        cx.new(|_| Dock {
            label: initial_label,
        })
    })
    .map_err(|error| OpenError::Gpui(error.into()))?;
    let scale = dock_window
        .update(cx, |_, window, cx| {
            configure_surface(window, PresenceSurface::Overlay, dock_events, cx)?;
            Ok::<_, platform::PresenceError>(window.scale_factor())
        })
        .map_err(|error| OpenError::Gpui(error.into()))??;
    let owner = cx.new(|cx| {
        listen(states, event_changes, cx);
        Presence {
            native,
            dock_window,
            dock,
            panel: None,
            events,
            state: LinkState::Connecting,
            link,
            layout: Layout {
                dock: [0; 4],
                panel: [0; 4],
            },
            panel_visibility: PanelVisibility::Hidden,
            scale,
            pointer_settle: None,
        }
    });
    owner.update(cx, |this, cx| this.position(Position::default(), cx))?;
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
    if let Some(delay) = quit_delay {
        let weak = owner.downgrade();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(delay).await;
            let _closed = weak.update(cx, |this, cx| this.quit(cx));
        })
        .detach();
    }
    cx.set_global(PresenceApp { _owner: owner });
    Ok(())
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
                    PresenceEvent::PointerChanged => this.pointer_changed(cx)?,
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
                    PresenceEvent::DismissPanel => this.set_panel(PanelVisibility::Hidden, cx)?,
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
                    let label = dock_label(state, cx);
                    if state == LinkState::Failed {
                        this.native.enable_retry();
                    } else {
                        this.native.disable_retry();
                    }
                    this.state = state;
                    this.dock.update(cx, |dock, cx| {
                        dock.label = label;
                        cx.notify();
                    });
                    if let Some((_, panel)) = &this.panel {
                        panel.update(cx, |panel, cx| {
                            panel.state = state;
                            cx.notify();
                        });
                    }
                })
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
}

fn quit_delay(args: impl Iterator<Item = String>) -> Result<Option<Duration>, OpenError> {
    let mut delay = None;
    for arg in args {
        if arg == "--smoke" {
            delay = Some(Duration::from_secs(10));
        } else if let Some(value) = arg.strip_prefix("--quit-after-ms=") {
            let millis = value.parse::<u64>().map_err(|_| OpenError::QuitDelay)?;
            if !(1..=120_000).contains(&millis) {
                return Err(OpenError::QuitDelay);
            }
            delay = Some(Duration::from_millis(millis));
        }
    }
    Ok(delay)
}

#[cfg(test)]
mod tests {
    use super::quit_delay;
    use std::time::Duration;

    #[test]
    fn diagnostic_launches_have_a_bounded_quit_deadline() {
        assert_eq!(
            quit_delay(["--smoke".to_owned()].into_iter()).unwrap(),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            quit_delay(["--quit-after-ms=75000".to_owned()].into_iter()).unwrap(),
            Some(Duration::from_secs(75))
        );
        for invalid in ["0", "120001", "-1", "later"] {
            assert!(quit_delay([format!("--quit-after-ms={invalid}")].into_iter()).is_err());
        }
    }
}
