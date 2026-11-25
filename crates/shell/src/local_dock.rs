//! The bounded shell surface for a platform without an admitted brain link.

use std::{
    cell::{Cell, RefCell},
    process::ExitCode,
    rc::Rc,
};

use gpui_kit::component::{ActiveTheme as _, button::Button};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Bounds, Context, InteractiveElement as _,
    IntoElement, KeyBinding, Menu, MenuItem, ParentElement as _, Render, Role,
    StatefulInteractiveElement as _, Styled as _, TitlebarOptions, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, px, size,
};
use kumochi_protocol::constants::DISPLAY_NAME;
use kumochi_shell_core::link::{self, LinkEvent, LinkHandle, LinkState};

use crate::strings::{Strings, Text};

#[cfg(target_os = "macos")]
mod termination;

gpui_kit::actions!(local_dock, [QuitLocalDock]);

/// The answer to one platform termination request: `true` lets the app terminate.
type Reply = Box<dyn FnOnce(bool)>;

/// The platform's quit after the release, which settles with its termination requests.
type PlatformQuit = Box<dyn Fn()>;

/// App-wide cleanup custody: the startup admission (the instance lock and anything that must
/// close before it) and what has to finish before it is released. The app's Quit action owns
/// it, not the dock view, so losing the view never drops the admission on the UI thread and
/// Quit still works without a window.
pub struct Custody {
    admission: Cell<Option<Box<dyn Send>>>,
    link: RefCell<Option<LinkHandle>>,
    window: Cell<Option<AnyWindowHandle>>,
    closing: Cell<bool>,
    /// The app itself asked to quit: Quit, a window close or a failed open.
    requested: Cell<bool>,
    /// The release finished, so nothing is left to close.
    released: Cell<bool>,
    /// A platform termination request still owed its answer.
    owed: RefCell<Option<Reply>>,
    /// Lets a platform request start the release; set once the dock opens.
    app: RefCell<Option<AsyncApp>>,
    /// Ends the app after the release where the platform asks before terminating.
    platform_quit: RefCell<Option<PlatformQuit>>,
}

impl std::fmt::Debug for Custody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Custody")
            .field("closing", &self.closing.get())
            .finish_non_exhaustive()
    }
}

impl Custody {
    /// Takes custody of an admitted startup state.
    #[must_use]
    pub fn new(admission: impl Send + 'static) -> Rc<Self> {
        Rc::new(Self {
            admission: Cell::new(Some(Box::new(admission))),
            link: RefCell::default(),
            window: Cell::default(),
            closing: Cell::default(),
            requested: Cell::default(),
            released: Cell::default(),
            owed: RefCell::default(),
            app: RefCell::default(),
            platform_quit: RefCell::default(),
        })
    }

    /// Sets the platform's quit, used after the release instead of the app's own. The platform
    /// then decides whether it must still ask to terminate: a request already in flight ends the
    /// app, and a new one would wait inside the quit for a reply it blocks.
    pub fn quit_through(&self, quit: impl Fn() + 'static) {
        *self.platform_quit.borrow_mut() = Some(Box::new(quit));
    }

    /// Answers a platform request to terminate the app, such as a quit from the macOS Dock.
    /// Before the dock opens the request is refused. While the admission is live the request
    /// starts the release, or joins the one under way, and is approved once it finishes. After
    /// the release it is approved at once, whether or not the dock is still there.
    /// Call it on the UI thread outside any `App` update, because it may start the release.
    pub fn answer_termination(self: &Rc<Self>, reply: impl FnOnce(bool) + 'static) {
        if self.released.get() {
            return reply(true);
        }
        let app = self.app.borrow().clone();
        if app.is_none() && !self.closing.get() {
            return reply(false);
        }
        if let Some(earlier) = self.owed.replace(Some(Box::new(reply))) {
            earlier(false);
        }
        if let Some(app) = app
            && !self.closing.get()
        {
            app.update(|cx| close(self, cx));
        }
    }

    /// Marks the release finished and answers a request owed an answer. Returns whether the app
    /// still has to quit on its own: only for its own quit, when no platform request was approved.
    fn finish(&self) -> bool {
        self.released.set(true);
        let Some(reply) = self.owed.take() else {
            return self.requested.get();
        };
        reply(true);
        false
    }
}

/// Runs the disconnected dock with the already admitted startup state.
/// An open failure and Quit both release the admission off the UI thread, because its owners
/// may block while they close.
/// The returned status applies only to backends whose event loop returns.
pub fn run(
    admission: impl Send + 'static,
    initialize: impl FnOnce(&mut App) + 'static,
) -> ExitCode {
    let custody = Custody::new(admission);
    let opened = Rc::new(Cell::new(false));
    let startup = Rc::clone(&opened);
    #[cfg(target_os = "macos")]
    let application = termination::application(&custody);
    #[cfg(not(target_os = "macos"))]
    let application = gpui_kit::application();
    application
        .with_assets(crate::assets::Assets)
        .run(move |cx| {
            initialize(cx);
            if open_with_rollback(cx, &custody).is_ok() {
                startup.set(true);
            }
        });
    if opened.get() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn open_with_rollback(cx: &mut App, custody: &Rc<Custody>) -> Result<AnyWindowHandle, OpenError> {
    open(cx, custody).inspect_err(|error| {
        tracing::error!(%error, "can't open the disconnected dock");
        request(custody, cx);
    })
}

/// An app-owned quit: the release, then the app quits on its own. After the release, such as when
/// the platform cancelled a termination it had been allowed, only the platform quit is left.
fn request(custody: &Rc<Custody>, cx: &App) {
    custody.requested.set(true);
    if !custody.released.get() {
        return close(custody, cx);
    }
    quit(custody, cx);
}

/// Ends the app after the release, through the platform's quit where it has one.
fn quit(custody: &Custody, cx: &App) {
    match custody.platform_quit.borrow().as_ref() {
        Some(quit) => quit(),
        None => cx.quit(),
    }
}

/// Begins the one release: finishes the link (once the dock opened) and drops the admission off
/// the UI thread, because the logger joins as it closes; then removes the window, if any, and
/// settles the quit on the UI thread once that has finished.
fn close(custody: &Rc<Custody>, cx: &App) {
    if custody.closing.get() {
        return;
    }
    let link = custody.link.borrow().clone();
    if let Some(link) = &link
        && !link.begin_quit()
    {
        return;
    }
    custody.closing.set(true);
    let admission = custody.admission.take();
    let window = custody.window.get();
    let custody = Rc::clone(custody);
    let finished = cx.background_executor().spawn(async move {
        if let Some(link) = link {
            link.finish_quit();
        }
        drop(admission);
    });
    cx.spawn(async move |cx| {
        finished.await;
        cx.update(|cx| {
            let ends = custody.finish();
            if let Some(window) = window
                && window
                    .update(cx, |_, window, _| window.remove_window())
                    .is_err()
            {
                tracing::debug!("the dock window was already gone");
            }
            if ends {
                quit(&custody, cx);
            }
        });
    })
    .detach();
}

/// Why the ordinary disconnected window could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// GPUI did not report a usable display.
    #[error("no usable display for the disconnected dock")]
    Display,
    /// The disconnected link could not report its initial state.
    #[error("could not prepare the disconnected link")]
    Link(#[from] std::io::Error),
    /// The disconnected link did not report its expected failure state.
    #[error("disconnected link did not report a failed state")]
    InitialState,
    /// GPUI refused to open the window.
    #[error("could not open the disconnected dock window")]
    Window,
}

/// Open the one disconnected dock after ordinary startup admission and UI setup.
///
/// # Errors
/// Refuses to open without a usable display or a delivered disconnected state.
pub fn open(cx: &mut App, custody: &Rc<Custody>) -> Result<AnyWindowHandle, OpenError> {
    let display = cx.primary_display().ok_or(OpenError::Display)?;
    let visible = display.visible_bounds();
    if visible.size.width <= px(0.) || visible.size.height <= px(0.) {
        return Err(OpenError::Display);
    }
    let bounds = Bounds::centered_at(
        visible.center(),
        size(px(360.), px(164.)).min(&visible.size),
    );
    let (events, receiver) = async_channel::bounded(1);
    let link = link::prepare_disconnected(&events)?;
    if !matches!(receiver.try_recv(), Ok(LinkEvent::State(LinkState::Failed))) {
        return Err(OpenError::InitialState);
    }
    drop((events, receiver));

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        display_id: Some(display.id()),
        titlebar: Some(TitlebarOptions {
            title: Some(DISPLAY_NAME.into()),
            ..TitlebarOptions::default()
        }),
        kind: WindowKind::Normal,
        show: true,
        focus: false,
        is_resizable: false,
        window_background: WindowBackgroundAppearance::Opaque,
        ..WindowOptions::default()
    };
    let (window, dock) = gpui_kit::open_window(options, cx, |window, cx| {
        let closer = Rc::clone(custody);
        window.on_window_should_close(cx, move |_, cx| {
            request(&closer, cx);
            false
        });
        cx.new(|_| LocalDock {
            custody: Rc::clone(custody),
        })
    })
    .map_err(|_| OpenError::Window)?;
    drop(dock);
    *custody.link.borrow_mut() = Some(link);
    custody.window.set(Some(window));

    *custody.app.borrow_mut() = Some(cx.to_async());
    let closer = Rc::clone(custody);
    cx.on_action(move |_: &QuitLocalDock, cx| request(&closer, cx));
    let quit = cx.global::<Strings>().get(Text::Quit);
    // The local dock runs only on macOS, so its quit shortcut is the macOS one by design.
    cx.bind_keys([KeyBinding::new("cmd-q", QuitLocalDock, None)]);
    cx.set_menus([Menu::new(DISPLAY_NAME).items([MenuItem::action(quit, QuitLocalDock)])]);
    Ok(window)
}

struct LocalDock {
    custody: Rc<Custody>,
}

impl Render for LocalDock {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let status = strings.get(Text::Failed);
        let quit = strings.get(Text::Quit);
        div()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .flex()
            .flex_col()
            .justify_between()
            .gap_3()
            .px_4()
            .py_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().w_6().h_1p5().rounded_full().bg(cx.theme().foreground))
                    .child(
                        div()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(DISPLAY_NAME),
                    )
                    .child(
                        div()
                            .id("local-dock-status")
                            .role(Role::Status)
                            .aria_label(status.clone())
                            .debug_selector({
                                let status = status.clone();
                                move || format!("local-dock-status {status}")
                            })
                            .child(status),
                    ),
            )
            .child(
                div().flex().justify_end().child(
                    Button::new("local-dock-quit")
                        .label(quit)
                        .debug_selector(|| "local-dock-quit".into())
                        .on_click(cx.listener(|dock, _, _, cx| request(&dock.custody, cx))),
                ),
            )
    }
}

#[cfg(test)]
mod tests;
