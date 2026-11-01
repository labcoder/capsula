//! The slice-0 status window: whether the shell reaches the brain, Retry and Quit. Slice 1
//! replaces it with the dock and the tray.

use async_channel::Receiver;
use capsula_protocol::constants;
use gpui_kit::component::button::Button;
use gpui_kit::{
    App, AppContext as _, Bounds, Context, IntoElement, ParentElement as _, Render, Styled as _,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, point, px, size,
};

use crate::link::{LinkHandle, LinkState};
use crate::platform;
use crate::strings::{Strings, Text};

/// The view: one status line that follows the link, Retry after a failure, and Quit.
#[derive(Debug)]
struct Status {
    state: LinkState,
    link: LinkHandle,
}

impl Status {
    /// Tells the brain off the UI thread, then quits.
    fn quit(&self, cx: &mut Context<'_, Self>) {
        let link = self.link.clone();
        let told = cx.background_executor().spawn(async move { link.quit() });
        cx.spawn(async move |_, cx| {
            told.await;
            cx.update(|cx| cx.quit());
        })
        .detach();
    }
}

impl Render for Status {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let text = match self.state {
            LinkState::Connecting => Text::Connecting,
            LinkState::Connected => Text::Connected,
            LinkState::Reconnecting => Text::Reconnecting,
            LinkState::Failed => Text::Failed,
        };
        let retry = (self.state == LinkState::Failed).then(|| {
            let link = self.link.clone();
            Button::new("retry")
                .label(strings.get(Text::Retry))
                .on_click(move |_, _, _| link.retry())
        });
        let quit = Button::new("quit")
            .label(strings.get(Text::Quit))
            .on_click(cx.listener(|status, _, _, cx| status.quit(cx)));
        div()
            .size_full()
            .p_4()
            .flex()
            .flex_col()
            .gap_2()
            .child(strings.get(text))
            .child(div().flex().gap_2().children(retry).child(quit))
    }
}

/// Why the status window couldn't open.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OpenError {
    /// GPUI couldn't create or update the window.
    #[error("GPUI couldn't open the window")]
    Gpui(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The window couldn't be shown without activating it.
    #[error(transparent)]
    Show(#[from] platform::ShowError),
}

/// Opens the status window without taking focus, following `states`.
///
/// # Errors
/// Fails when GPUI can't open the window or it can't be shown.
pub(crate) fn open(
    states: Receiver<LinkState>,
    link: LinkHandle,
    cx: &mut App,
) -> Result<(), OpenError> {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(80.0), px(80.0)),
            size(px(320.0), px(140.0)),
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some(constants::DISPLAY_NAME.into()),
            ..Default::default()
        }),
        show: false,
        focus: false,
        ..Default::default()
    };
    let (handle, _) = gpui_kit::open_window(options, cx, |window, cx| {
        let view = cx.new(|cx| {
            cx.spawn(async move |this, cx| {
                while let Ok(state) = states.recv().await {
                    let updated = this.update(cx, |status: &mut Status, cx| {
                        status.state = state;
                        cx.notify();
                    });
                    if updated.is_err() {
                        break;
                    }
                }
            })
            .detach();
            Status {
                state: LinkState::Connecting,
                link,
            }
        });
        // It's slice 0's only window, so closing it is Quit; otherwise the brain would see a
        // crash and relaunch the shell.
        let closing = view.downgrade();
        window.on_window_should_close(cx, move |_, cx| {
            closing.update(cx, |status, cx| status.quit(cx)).is_err()
        });
        view
    })
    .map_err(|e| OpenError::Gpui(e.into()))?;
    handle
        .update(cx, |_, window, cx| {
            platform::show_without_activating(window, cx)
        })
        .map_err(|e| OpenError::Gpui(e.into()))??;
    Ok(())
}
