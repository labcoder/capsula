//! `kumochi`, Kumochi's shell. It has no console window.
#![cfg_attr(not(test), windows_subsystem = "windows")]

#[cfg(test)]
#[path = "../../scene/tests/support/allocation.rs"]
#[expect(
    dead_code,
    reason = "shared allocator helpers serve several test binaries"
)]
mod allocation;
#[cfg(test)]
#[path = "../../../tests/support/scratch.rs"]
mod scratch;

mod confirmation;
#[cfg(test)]
mod github_link_tests;
mod platform;
mod presence;
mod presenter;
#[cfg(target_os = "macos")]
#[expect(
    dead_code,
    reason = "dormant until the connected Mac shell admits screen capture"
)]
mod screen_access;

use std::process::ExitCode;

use kumochi_protocol::constants;
use kumochi_protocol::message::{AudioSession, Narration};
use kumochi_shell::presence_platform::{Frontend, select_frontend};
use kumochi_shell::{assets, strings, theme};
use kumochi_shell_core::{
    accounts, chat_notice, claude_accounts, drawing, escape, input, link, onboarding, runs,
    settings, sign_in_mark, startup, threads,
};
use strings::Strings;

fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1).peekable();
    let Ok(frontend) = select_frontend(&mut arguments) else {
        return ExitCode::FAILURE;
    };
    let launch = presence::Launch::parse(arguments);
    #[cfg(debug_assertions)]
    if let Ok(flags) = &launch
        && let Some(state) = flags.accounts_fixture
    {
        let delay = flags
            .quit_delay
            .expect("fixture parsing requires a deadline");
        presence::run_accounts_fixture(state, flags.accounts_appearance, delay);
        return ExitCode::SUCCESS;
    }
    #[cfg(not(target_os = "macos"))]
    let (flags, admitted) = match startup::start(
        kumochi_shell_core::instance::InstanceLock::admit_current_session,
        launch,
        || kumochi_shell_core::logging::init(constants::SHELL_LOG),
    ) {
        Ok(started) => started,
        Err(refused) => return refused.exit_code(),
    };
    // macOS logs into the lock's admitted storage, so the lock comes first and its refusals go unlogged.
    #[cfg(target_os = "macos")]
    let (flags, admitted) = {
        let instance = match startup::acquire() {
            Ok(lock) => lock,
            Err(refused) => return refused.exit_code(),
        };
        let Some(admitted) = platform::Admitted::start(instance) else {
            return ExitCode::FAILURE;
        };
        let Some(flags) = startup::logged_flags(launch, || Ok(())) else {
            return ExitCode::FAILURE;
        };
        (flags, admitted)
    };
    tracing::info!(
        build = kumochi_protocol::BUILD_ID,
        identity = constants::DATA_DIR,
        edition = constants::EDITION,
        "shell started"
    );
    match frontend {
        Frontend::Connected => {
            let _admitted = admitted;
            run_connected(flags)
        }
        Frontend::Local => kumochi_shell::run_local_dock(admitted, initialize_gpui),
    }
}

fn initialize_gpui(cx: &mut gpui_kit::App) {
    gpui_kit::init(cx);
    crate::theme::init(cx);
    cx.set_global(Strings::english());
}

/// The ordinary launch's link: queued audio whose narrated steps the drawing conducts.
fn ordinary_link(
    events: async_channel::Sender<link::LinkEvent>,
) -> std::io::Result<(link::LinkHandle, link::LinkStart)> {
    link::prepare(events, AudioSession::QueuedRequired, Narration::Conducted)
}

#[cfg(all(test, windows, debug_assertions))]
#[test]
fn the_ordinary_launch_conducts_two_narrated_steps() {
    let (events, receiver) = async_channel::unbounded();
    let prepared = ordinary_link(events).expect("the ordinary link prepares");
    link::conduct_fixture::conducts_two_steps("ordinary-two-steps", prepared, receiver);
}

fn run_connected(flags: presence::Launch) -> ExitCode {
    let (events, link_events) = async_channel::unbounded();
    let (link, start) = match ordinary_link(events) {
        Ok(prepared) => prepared,
        Err(error) => {
            tracing::error!(%error, "can't prepare the link to the brain");
            return ExitCode::FAILURE;
        }
    };
    let observer = match presence::Observer::prepare(&flags) {
        Ok(observer) => observer,
        Err(error) => {
            tracing::error!(%error, "can't create the --gate-observer file");
            return ExitCode::FAILURE;
        }
    };
    let diagnostics = match presence::Diagnostics::prepare() {
        Ok(diagnostics) => diagnostics,
        Err(error) => {
            tracing::error!(%error, "can't create the --input-diagnostics file");
            return ExitCode::FAILURE;
        }
    };
    let platform = gpui_kit::platform::current_platform(false);
    let drawing_text = platform.text_system();
    gpui_kit::Application::with_platform(platform)
        .with_assets(crate::assets::Assets)
        .run(move |cx| {
            initialize_gpui(cx);
            // The keyboard is armed inside `open`, so the link starts only afterwards (v1 §2.2).
            let started =
                presence::open(link_events, link.clone(), &flags, diagnostics, observer, cx)
                    .map_err(|error| tracing::error!(%error, "can't open the dock"))
                    .and_then(|()| {
                        start
                            .spawn(crate::presenter::text_profiles(cx, drawing_text))
                            .map_err(|error| tracing::error!(%error, "can't start the link"))
                    });
            if started.is_err() {
                link.begin_quit();
                let diagnostics = presence::diagnostics::release(cx);
                let observer = presence::observer::release(cx);
                let told = cx.background_executor().spawn(async move {
                    link.finish_quit();
                    drop(diagnostics);
                    if let Some(observer) = observer {
                        observer.finish();
                    }
                });
                cx.spawn(async move |cx| {
                    told.await;
                    cx.update(|cx| cx.quit());
                })
                .detach();
            }
        });
    ExitCode::SUCCESS
}
