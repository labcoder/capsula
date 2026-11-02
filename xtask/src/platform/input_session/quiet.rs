//! Bounded hidden-window observation using the fixture's unchanged raw-input guard.

use std::cell::Cell;
use std::io::{Write as _, stdout};
use std::time::{Duration, Instant};

use anyhow::{Result, bail, ensure};
use kumochi_protocol::constants::OBJECT_PREFIX;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::UI::Input::{RIDEV_INPUTSINK, RIDEV_REMOVE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, MSG,
    PostQuitMessage, RegisterClassW, SetTimer, TranslateMessage, UnregisterClassW, WINDOW_STYLE,
    WM_CLOSE, WM_INPUT, WM_TIMER, WM_WTSSESSION_CHANGE, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WTS_SESSION_LOCK,
};
use windows::core::{HSTRING, PCWSTR};

use super::fixture::raw_devices;
use super::guard::read_input;

#[derive(Debug, Clone, Copy)]
enum Failure {
    Output,
    RawRead { output_failed: bool },
    SessionLock { output_failed: bool },
}

thread_local! {
    static FAILURE: Cell<Option<Failure>> = const { Cell::new(None) };
}

fn fail(failure: Failure) {
    FAILURE.set(FAILURE.get().or(Some(failure)));
    // SAFETY: a failed observation stops only this hidden observer's own message loop.
    unsafe { PostQuitMessage(1) };
}

fn outcome(failure: Option<Failure>) -> Result<()> {
    match failure {
        Some(Failure::Output) => bail!("input-quiet output failed"),
        Some(Failure::RawRead { output_failed }) => {
            bail!("input-quiet raw-input guard read failed (error output failed: {output_failed})")
        }
        Some(Failure::SessionLock { output_failed }) => {
            bail!("input-quiet session locked (error output failed: {output_failed})")
        }
        None => Ok(()),
    }
}

unsafe extern "system" fn quiet_proc(
    window: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    if message == WM_INPUT {
        let failure = match read_input(lp) {
            Ok(observation) => writeln!(
                stdout().lock(),
                "raw_input input={:?} stops_guard={}",
                observation.input,
                !observation.exempt,
            )
            .err()
            .map(|_| Failure::Output),
            Err(error) => Some(Failure::RawRead {
                output_failed: writeln!(
                    stdout().lock(),
                    "raw_read_error stops_guard=true error={error}"
                )
                .is_err(),
            }),
        };
        if let Some(failure) = failure {
            fail(failure);
        }
    }
    if message == WM_WTSSESSION_CHANGE
        && wp.0 == usize::try_from(WTS_SESSION_LOCK).expect("session event fits usize")
    {
        fail(Failure::SessionLock {
            output_failed: writeln!(stdout().lock(), "session_lock stops_guard=true").is_err(),
        });
    }
    if message == WM_TIMER || message == WM_CLOSE {
        // SAFETY: this stops only the hidden observer's own message loop.
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }
    // SAFETY: default processing also performs the required raw-input message cleanup.
    unsafe { DefWindowProcW(window, message, wp, lp) }
}

fn duration(seconds: u64) -> Result<Duration> {
    ensure!(
        (1..=120).contains(&seconds),
        "input-quiet needs 1..120 seconds"
    );
    Ok(Duration::from_secs(seconds))
}

pub(crate) fn quiet(seconds: u64) -> Result<()> {
    let duration = duration(seconds)?;
    FAILURE.set(None);
    // SAFETY: this reads the observer executable's own loaded module.
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }?.into();
    let name = HSTRING::from(format!("{OBJECT_PREFIX}-input-quiet"));
    let class = WNDCLASSW {
        lpfnWndProc: Some(quiet_proc),
        hInstance: instance,
        lpszClassName: PCWSTR(name.as_ptr()),
        ..Default::default()
    };
    // SAFETY: the class has a static callback and its name remains live until unregistration.
    let registered = unsafe { RegisterClassW(&raw const class) };
    ensure!(registered != 0, "input-quiet class registration failed");
    // SAFETY: creates an invisible nonactivating window, without foreground or input changes.
    let created = unsafe {
        CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            &name,
            &HSTRING::new(),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
    };
    let result = match created {
        Ok(window) => {
            // SAFETY: notifications target only this hidden window in the current session.
            let notifications =
                unsafe { WTSRegisterSessionNotification(window, NOTIFY_FOR_THIS_SESSION) };
            let subscribed = notifications.is_ok();
            let result = notifications
                .map_err(Into::into)
                .and_then(|()| observe(window, duration));
            let result = cleanup(result, raw_devices(HWND::default(), RIDEV_REMOVE));
            let result = if subscribed {
                // SAFETY: only this observer's successful notification registration is released.
                cleanup(
                    result,
                    unsafe { WTSUnRegisterSessionNotification(window) }.map_err(Into::into),
                )
            } else {
                result
            };
            // SAFETY: this hidden window is still owned by the current thread.
            cleanup(result, unsafe { DestroyWindow(window) }.map_err(Into::into))
        }
        Err(error) => Err(error.into()),
    };
    // SAFETY: unregisters only this observer's class; a remaining window makes the API fail.
    cleanup(
        result,
        unsafe { UnregisterClassW(&name, Some(instance)) }.map_err(Into::into),
    )
}

fn observe(window: HWND, duration: Duration) -> Result<()> {
    let until = Instant::now() + duration;
    raw_devices(window, RIDEV_INPUTSINK)?;
    // SAFETY: the hidden window owns timer 1, which wakes an otherwise quiet message loop.
    let timer = unsafe { SetTimer(Some(window), 1, u32::try_from(duration.as_millis())?, None) };
    ensure!(timer != 0, "input-quiet timer setup failed");
    let mut message = MSG::default();
    while Instant::now() < until {
        // SAFETY: message is writable and receives only this observer thread's queued messages.
        let got = unsafe { GetMessageW(&raw mut message, None, 0, 0) }.0;
        ensure!(got >= 0, "input-quiet message loop failed");
        if got == 0 {
            outcome(FAILURE.get())?;
            ensure!(message.wParam.0 == 0, "input-quiet output failed");
            break;
        }
        // SAFETY: this message was obtained from the observer thread's queue.
        let _translated = unsafe { TranslateMessage(&raw const message) };
        // SAFETY: dispatches the unchanged message to this thread's own window procedure.
        unsafe { DispatchMessageW(&raw const message) };
        outcome(FAILURE.get())?;
    }
    outcome(FAILURE.get())
}

fn cleanup(result: Result<()>, cleaned: Result<()>) -> Result<()> {
    match (result, cleaned) {
        (Ok(()), done) => done,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("input-quiet cleanup also failed: {cleanup}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Failure, duration, outcome};
    use std::time::Duration;

    #[test]
    fn observation_is_bounded_to_one_through_120_seconds() {
        assert!(duration(0).is_err());
        assert_eq!(duration(1).unwrap(), Duration::from_secs(1));
        assert_eq!(duration(120).unwrap(), Duration::from_secs(120));
        assert!(duration(121).is_err());
        assert!(duration(u64::MAX).is_err());
    }

    #[test]
    fn a_read_failure_or_lock_cannot_be_reported_as_a_quiet_pass_or_output_failure() {
        assert!(outcome(None).is_ok());
        for output_failed in [false, true] {
            assert_eq!(
                outcome(Some(Failure::RawRead { output_failed }))
                    .unwrap_err()
                    .to_string(),
                format!(
                    "input-quiet raw-input guard read failed (error output failed: {output_failed})"
                )
            );
            assert_eq!(
                outcome(Some(Failure::SessionLock { output_failed }))
                    .unwrap_err()
                    .to_string(),
                format!("input-quiet session locked (error output failed: {output_failed})")
            );
        }
        assert_eq!(
            outcome(Some(Failure::Output)).unwrap_err().to_string(),
            "input-quiet output failed"
        );
    }
}
