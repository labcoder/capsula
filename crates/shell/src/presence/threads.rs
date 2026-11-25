//! Lifetime, focus, and page requests for the anchored threads surface.

use std::rc::Rc;

use gpui_kit::{AnyWindowHandle, AppContext as _, Context, Entity, Subscription, Window};
use kumochi_protocol::message::{ThreadId, ThreadRow};

use super::text::{Hide, Typing};
use super::thread_view::ThreadsView;
use super::{
    Foreground, OpenError, PanelVisibility, Presence, PresenceEvent, PresenceSurface,
    configure_surface, options, platform, send,
};
use crate::strings::{Strings, Text};

/// The initial native window size, in interface rems [guess].
pub(super) const PANEL_REMS: [f32; 2] = [48.0, 34.0];

pub(super) struct ThreadsPanel {
    pub(super) window: AnyWindowHandle,
    pub(super) view: Entity<ThreadsView>,
    pub(super) rename: super::escape::RenameEdit,
    pub(super) typing: Typing,
    pub(super) focused: bool,
    pub(super) bounds: [i32; 4],
    previous: Option<Foreground>,
    activation: Option<Subscription>,
}

#[cfg(test)]
impl ThreadsPanel {
    /// A hidden tasks panel with its real view in a GPUI test window, without
    /// `create_threads`' native surface, capture or activation wiring.
    pub(super) fn detached(
        model: Entity<crate::threads::Threads>,
        cx: &mut Context<'_, Presence>,
    ) -> Self {
        let (events, _closed) = async_channel::unbounded();
        let owner = cx.weak_entity();
        let (window, view) =
            gpui_kit::open_window(gpui_kit::WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| ThreadsView::new(model, owner, None, events, window, cx))
            })
            .expect("a GPUI test window opens");
        Self {
            window,
            rename: Rc::clone(&view.read(cx).rename),
            view,
            typing: Typing::default(),
            focused: false,
            bounds: [0; 4],
            previous: None,
            activation: None,
        }
    }
}

impl Presence {
    pub(super) fn threads_open(&self) -> bool {
        self.thread_panel
            .as_ref()
            .is_some_and(|panel| panel.typing.visible)
    }

    pub(super) fn threads_focused(&self) -> bool {
        self.thread_panel
            .as_ref()
            .is_some_and(|panel| panel.typing.visible && panel.focused)
    }

    pub(super) fn request_pages(&mut self, cx: &mut Context<'_, Self>) {
        let mut requests = Vec::new();
        for model in [&self.threads, &self.chat] {
            requests.extend(model.update(cx, |state, cx| {
                let requests = state.requests();
                if !requests.is_empty() {
                    cx.notify();
                }
                requests
            }));
        }
        if self.threads.read(cx).exhausted() || self.threads.read(cx).cards_broken() {
            if let Some(session) = &self.session {
                self.link.end(session);
            }
            return;
        }
        for request in requests {
            if !self
                .session
                .as_ref()
                .is_some_and(|session| self.link.send_data(session, request))
            {
                if let Some(session) = &self.session {
                    self.link.end(session);
                }
                for model in [&self.threads, &self.chat] {
                    model.update(cx, |state, cx| {
                        state.disconnected();
                        cx.notify();
                    });
                }
                tracing::debug!("a page request couldn't reach its connection");
                break;
            }
        }
    }

    pub(super) fn select_thread(&mut self, thread: Option<ThreadId>, cx: &mut Context<'_, Self>) {
        self.cancel_voice(cx);
        self.threads.update(cx, |state, cx| {
            state.select(thread);
            cx.notify();
        });
        self.request_pages(cx);
    }

    pub(super) fn switch_thread(&mut self, row: &ThreadRow, cx: &mut Context<'_, Self>) {
        self.cancel_voice(cx);
        if self.runs.select_row(row) {
            self.select_thread(Some(row.id), cx);
            self.refresh_conversation(cx);
        }
    }

    pub(super) fn back_to_kumo(&mut self, cx: &mut Context<'_, Self>) {
        self.cancel_voice(cx);
        self.runs.back_to_kumo();
        let main = self
            .threads
            .read(cx)
            .settings
            .as_ref()
            .map(|settings| settings.main_thread);
        self.select_thread(main, cx);
        self.refresh_conversation(cx);
    }

    pub(super) fn new_thread(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        self.cancel_voice(cx);
        if !self.runs.select_thread(None) {
            return Ok(());
        }
        self.select_thread(None, cx);
        self.dismiss_threads(cx)?;
        self.open_text(cx)
    }

    pub(super) fn open_threads(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        self.dismiss_settings(cx)?;
        self.dismiss_text(cx)?;
        if self.thread_panel.is_none() {
            self.thread_panel = Some(self.create_threads(cx)?);
        }
        self.protect_surface(super::exclusion::Slot::Threads, cx)?;
        self.pointer_settle = None;
        self.set_panel(PanelVisibility::Hidden, cx)?;
        let area = platform::work_area()?;
        let panel = self
            .thread_panel
            .as_mut()
            .expect("the threads window was created above");
        let rem = panel
            .window
            .update(cx, |_, window, _| f32::from(window.rem_size()))
            .map_err(|error| OpenError::Gpui(error.into()))?;
        panel.bounds = self.layout.anchored_panel(
            area,
            PANEL_REMS.map(|value| super::scaled(value * rem, self.scale)),
        );
        let bounds = panel.bounds;
        let stop = self.escape_hold.shows_stop();
        panel.view.update(cx, |view, cx| {
            view.stop = stop;
            cx.notify();
        });
        let list = panel.view.read(cx).list.clone();
        panel.typing.open();
        panel
            .window
            .update(cx, |_, window, cx| {
                platform::place_presence_window(window, bounds)?;
                window.bounds_changed(cx);
                match platform::activate_presence_window(window) {
                    Ok(previous) => {
                        if previous.is_some() {
                            panel.previous = previous;
                        }
                        list.update(cx, |list, cx| list.focus(window, cx));
                    }
                    Err(error) => tracing::warn!(%error, "can't focus the threads panel"),
                }
                Ok::<_, platform::PresenceError>(())
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        self.threads.update(cx, |state, cx| {
            state.show();
            cx.notify();
        });
        self.place_drawings(cx.weak_entity(), cx);
        self.request_pages(cx);
        Ok(())
    }

    fn create_threads(&mut self, cx: &mut Context<'_, Self>) -> Result<ThreadsPanel, OpenError> {
        let model = self.threads.clone();
        let owner = cx.weak_entity();
        let error = self.error.clone();
        let events = self.events.clone();
        let close_events = self.events.clone();
        let (window, view) = gpui_kit::open_window(options(), cx, |window, cx| {
            window.set_window_title(&cx.global::<Strings>().get(Text::Threads));
            window.on_window_should_close(cx, move |_, _| {
                send(&close_events, PresenceEvent::DismissPanel);
                false
            });
            cx.new(|cx| ThreadsView::new(model, owner, error, events, window, cx))
        })
        .map_err(|error| OpenError::Gpui(error.into()))?;
        let events = self.events.clone();
        window
            .update(cx, |_, window, cx| {
                configure_surface(window, PresenceSurface::Text, events, cx)?;
                self.input_windows
                    .bind(crate::presenter::overlay_input::Layer::Panels, window)?;
                Ok::<_, platform::PresenceError>(())
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        let owner = cx.weak_entity();
        cx.defer(move |cx| {
            let result = window.update(cx, |_, window, cx| {
                owner.update(cx, |this, cx| this.observe_threads_activation(window, cx))
            });
            if let Err(error) = result {
                tracing::error!(%error, "can't observe threads panel activation");
            }
        });
        Ok(ThreadsPanel {
            window,
            rename: Rc::clone(&view.read(cx).rename),
            view,
            typing: Typing::default(),
            focused: false,
            bounds: [0; 4],
            previous: None,
            activation: None,
        })
    }

    fn observe_threads_activation(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            this.threads_activation(window, cx);
        });
        if let Some(panel) = &mut self.thread_panel {
            panel.activation = Some(activation);
        }
    }

    fn threads_activation(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let active = window.is_window_active();
        if let Some(panel) = &mut self.thread_panel {
            panel.focused = active;
            if active {
                panel.typing.activated();
            }
        }
        self.focus_changed(cx);
        if active {
            return;
        }
        let owner = cx.weak_entity();
        cx.defer(move |cx| {
            if let Ok(Err(error)) =
                owner.update(cx, |this, cx| this.hide_threads(Hide::Deactivated, cx))
            {
                tracing::error!(%error, "can't hide the inactive threads panel");
            }
        });
    }

    pub(super) fn dismiss_threads(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        self.hide_threads(Hide::Dismiss, cx)
    }

    fn hide_threads(&mut self, reason: Hide, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        let Some(panel) = &mut self.thread_panel else {
            return Ok(());
        };
        let shown = (panel.typing, panel.focused, panel.previous);
        if !panel.typing.close(reason) {
            return Ok(());
        }
        panel.focused = false;
        let previous = panel
            .previous
            .take()
            .filter(|_| reason.restores_foreground());
        let hidden = panel.window.update(cx, |_, window, _| {
            if let Some(previous) = previous {
                platform::restore_foreground(window, previous);
            }
            platform::hide_presence_window(window)
        });
        // A panel still on screen must stay open, so Esc and its close button can try again.
        if !matches!(hidden, Ok(Ok(()))) {
            (panel.typing, panel.focused, panel.previous) = shown;
        }
        hidden.map_err(|error| OpenError::Gpui(error.into()))??;
        self.threads.update(cx, |state, cx| {
            state.hide();
            cx.notify();
        });
        self.place_drawings(cx.weak_entity(), cx);
        self.request_pages(cx);
        self.pointer_changed(cx)
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;
    use kumochi_protocol::message::ThreadId;

    use super::{OpenError, Presence, ThreadsPanel};

    #[gpui::test]
    fn a_tasks_panel_that_failed_to_hide_stays_open_and_esc_still_closes_it(
        cx: &mut TestAppContext,
    ) {
        let _area = crate::platform::work_area_fixture::set([0, 0, 2560, 1400]);
        let owner = crate::presence::test_fixture::presence(cx);
        let window = owner.update(cx, |owner, cx| {
            owner.thread_panel = Some(ThreadsPanel::detached(owner.threads.clone(), cx));
            owner.select_thread(Some(ThreadId(3)), cx);
            owner.open_threads(cx).expect("open tasks");
            let panel = owner.thread_panel.as_mut().expect("tasks fixture");
            // As its window's activation reports once it's in front.
            panel.focused = true;
            panel.typing.activated();
            panel.window
        });
        // GPUI refuses to update a window from inside its own update, as a click on it would.
        let hidden = window
            .update(cx, |_, _, cx| owner.update(cx, Presence::dismiss_threads))
            .expect("tasks window");
        assert!(matches!(hidden, Err(OpenError::Gpui(_))));
        owner.update(cx, |owner, cx| {
            assert!(owner.threads_focused(), "Tasks is still open and focused");
            assert_eq!(owner.threads.read(cx).open, Some(ThreadId(3)));
            owner.escape(cx).expect("Esc");
            assert!(!owner.threads_open(), "Esc hid Tasks");
            assert_eq!(owner.threads.read(cx).open, None);
        });
    }
}
