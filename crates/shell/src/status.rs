//! The slice-0 status window: what the shell is doing. Slice 1 replaces it with the dock.

use capsula_protocol::constants;
use gpui_kit::{
    App, AppContext as _, Bounds, Context, IntoElement, ParentElement as _, Render, Styled as _,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, point, px, size,
};

use crate::platform;
use crate::strings::{Strings, Text};

/// The view: one status line.
#[derive(Debug)]
struct Status;

impl Render for Status {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .size_full()
            .p_4()
            .child(cx.global::<Strings>().get(Text::StatusStarting))
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

/// Opens the status window without taking focus.
///
/// # Errors
/// Fails when GPUI can't open the window or it can't be shown.
pub(crate) fn open(cx: &mut App) -> Result<(), OpenError> {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(80.0), px(80.0)),
            size(px(320.0), px(120.0)),
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some(constants::DISPLAY_NAME.into()),
            ..Default::default()
        }),
        show: false,
        focus: false,
        ..Default::default()
    };
    let (handle, _) = gpui_kit::open_window(options, cx, |_, cx| cx.new(|_| Status))
        .map_err(|e| OpenError::Gpui(e.into()))?;
    handle
        .update(cx, |_, window, cx| {
            platform::show_without_activating(window, cx)
        })
        .map_err(|e| OpenError::Gpui(e.into()))??;
    Ok(())
}
