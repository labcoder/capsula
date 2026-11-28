//! The shell on Windows: the GPUI shell's start-up, then a `NativeShell` app with one hidden main
//! window and no view, whose root runs the link's pump and the one quit.

use std::process::ExitCode;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use kumochi_protocol::constants;
use kumochi_shell_core::instance::InstanceLock;
use kumochi_shell_core::logging;
use kumochi_shell_core::startup::{self, Refused};
use nsh_core::desktop::MainWindowOptions;
use nsh_core::element::desktop::{main_window, on_quit_request, quit};
use nsh_core::element::{
    Component, IntoElement, NoEvent, Owner, Props, Scope, blocking, spawn, timer, view,
};
use nsh_platform_windows::{AppOptions, AppSource, CacheOptions, Root, run_app};

use super::backstop::{Backstop, Order};
use super::shell::{self, Act, Begun, Quit};
use crate::launch::{Launch, LaunchError};
use crate::ui::theme::{self, Look};

/// The backstop's handle for the root, since props take only values the engine can decode.
static BACKSTOP: OnceLock<Backstop> = OnceLock::new();

/// The root's one prop: the milliseconds left of the deadline, if there is one, when the engine
/// mounts the root, since props take numbers, not an `Instant`. The view's timer starts one
/// commit later.
#[derive(Clone, PartialEq, Props)]
struct EmptyAppProps {
    quit_after_ms: Option<u64>,
}

/// An app with nothing to show yet. Its tasks follow the link and quit the app once.
struct EmptyApp;

impl Component for EmptyApp {
    type Props = EmptyAppProps;
    type Event = NoEvent;

    fn view(props: EmptyAppProps, _: Scope<NoEvent>) -> impl IntoElement {
        let backstop = BACKSTOP
            .get()
            .cloned()
            .expect("`run` keeps the backstop before the app starts");
        // The quit task belongs to the root, not to the handler that asked: a close press runs in
        // an effect whose next run would cancel what it spawned.
        let root = Owner::current();
        let (link, events) = shell::started();
        let owner = Rc::new(Quit::new(backstop.clone(), link.clone()));
        let asker = |owner: &Rc<Quit>| {
            let owner = Rc::clone(owner);
            move || ask_quit(&owner, root)
        };
        let close = asker(&owner);
        main_window().on_close_press(move |_| close());
        on_quit_request(asker(&owner));
        if let Some(left) = props.quit_after_ms {
            let ask = asker(&owner);
            spawn(async move {
                timer(Duration::from_millis(left)).await;
                ask();
            });
        }
        let failed = link.is_none();
        spawn(shell::pump(link, events, move |act| match act {
            Act::Opened => backstop.tell(Order::Connected(Instant::now())),
            Act::Reconnecting => backstop.tell(Order::Reconnecting(Instant::now())),
            Act::Quit => ask_quit(&owner, root),
            // Detached: ending waits briefly on the output owner's lock, never on this thread.
            Act::End(link, session) => drop(blocking(move || {
                link.end(&session);
                tracing::warn!("the scene refusal was refused; ending the connection");
            })),
        }));
        if failed {
            drop(blocking(|| tracing::error!("can't start the link")));
        }
        view()
    }
}

/// Asks the one quit. The first ask waits for the link's quit on the blocking pool, since it can
/// take 13 s and quit hooks get 200 ms, then calls `quit()`, once, in a task `root` owns.
fn ask_quit(owner: &Quit, root: Owner) {
    let Some(begun) = owner.begin() else {
        return;
    };
    root.run(|| {
        spawn(async move {
            if let Begun::Finish(link) = begun {
                // Quit goes on however the wait ended.
                let _ended = blocking(move || link.finish_quit()).await;
            }
            quit();
        })
    });
}

/// The GPUI shell's own start: the same lock and log, so the two shells exclude each other.
pub(crate) fn start(
    launch: Result<Launch, LaunchError>,
) -> Result<(Launch, InstanceLock), ExitCode> {
    startup::start(InstanceLock::admit_current_session, launch, || {
        logging::init(constants::SHELL_LOG)
    })
    .map_err(Refused::exit_code)
}

/// Runs the app until it quits, then ends the process: 0 when its message loop ended with 0, else 1.
pub(crate) fn run(deadline: Option<Instant>, backstop: Backstop) -> ExitCode {
    BACKSTOP
        .set(backstop)
        .expect("the app runs once in a process");
    let mut window = MainWindowOptions::default();
    window.default_visible = Some(false);
    // A close press asks the one quit instead of closing at once.
    window.attributes.close_on_press = Some(false);
    // The engine makes the props as it mounts the root, which can come late: only what's left then,
    // so the quit can trail the deadline by the one commit before the view's timer starts.
    let root = Root::new::<EmptyApp>(move || EmptyAppProps {
        quit_after_ms: deadline.map(|deadline| {
            let left = deadline.saturating_duration_since(Instant::now());
            u64::try_from(left.as_millis()).unwrap_or(u64::MAX)
        }),
    })
    .app_id(constants::SHELL_APP_ID)
    .main_window(window)
    .appearance(Look::default().appearance());
    // Read here, on the main thread, since this crate never logs on the engine thread.
    let root = if let Ok(tokens) = theme::tokens() {
        root.tokens(tokens)
    } else {
        tracing::error!("can't read the theme; drawing with NativeShell's default");
        root
    };
    // shortcut: NativeShell's own `log` lines go nowhere, since their content (paths, foreign
    // error text) isn't vetted for Kumochi's log; bridge them through an allow-list of targets
    // once the shell shows something worth diagnosing.
    run_app(
        AppSource::Rust(root),
        AppOptions {
            app_id: constants::SHELL_APP_ID.to_owned(),
            title: constants::DISPLAY_NAME.to_owned(),
            skip_taskbar: true,
            cache: CacheOptions::off(),
            ..AppOptions::default()
        },
    )
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    use super::*;

    #[test]
    fn the_binary_has_the_name_the_staging_task_copies() {
        assert_eq!(
            concat!(env!("CARGO_BIN_NAME"), ".exe"),
            constants::NSH_SHELL_EXE
        );
    }

    #[test]
    fn an_argument_that_isnt_unicode_is_unknown() {
        // The flag with a valid delay, then an unpaired surrogate: a Windows argument, not Unicode.
        let mut arg: Vec<u16> = "--quit-after-ms=2000".encode_utf16().collect();
        arg.push(0xD800);
        assert_eq!(
            Launch::parse(std::iter::once(OsString::from_wide(&arg))),
            Err(LaunchError::Unknown)
        );
    }
}
