//! Keyboard-layout snapshot and restoration around the fixture's layout requests.

use anyhow::{Context as _, Result, bail, ensure};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardLayout, GetKeyboardLayoutList, HKL};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

use super::registry::Registry;
use super::{Layout, Session, Window, fixture, request};

/// What the layouts looked like before the layouts phase. Capturing never fails, so cleanup
/// always has something to compare against; a part that couldn't be read fails the restore.
#[derive(Debug)]
pub(crate) struct Layouts {
    registry: Registry,
    loaded: Result<Vec<HKL>>,
    fixture: Option<HKL>,
}

fn loaded() -> Result<Vec<HKL>> {
    // SAFETY: querying the list size does not change layouts.
    let count = unsafe { GetKeyboardLayoutList(None) };
    ensure!(count > 0, "cannot read the loaded keyboard layouts");
    let mut layouts = vec![HKL::default(); usize::try_from(count)?];
    // SAFETY: layouts is writable for the advertised number of handles.
    let read = unsafe { GetKeyboardLayoutList(Some(&mut layouts)) };
    ensure!(
        read == count,
        "keyboard layout list changed while reading it"
    );
    Ok(layouts)
}

/// The fixture thread's active layout, if the fixture is still the window it was.
fn fixture_layout(fixture: Window) -> Option<HKL> {
    fixture.validate().ok()?;
    // SAFETY: only the PID-validated fixture's thread is queried.
    let thread = unsafe { GetWindowThreadProcessId(fixture.hwnd, None) };
    // SAFETY: reads that thread's active layout without changing it.
    (thread != 0).then(|| unsafe { GetKeyboardLayout(thread) })
}

impl Layouts {
    /// Reads the registry keys, the loaded layouts and the fixture's layout; sends nothing.
    pub(crate) fn capture(session: &Session) -> Self {
        Self {
            registry: Registry::snapshot(),
            loaded: loaded(),
            fixture: fixture_layout(session.fixture),
        }
    }

    pub(crate) fn load(session: &Session, selection: Layout) -> Result<()> {
        session.guard()?;
        let selection = match selection {
            Layout::German => 0,
            Layout::Spanish => 1,
        };
        let loaded = request(
            session.fixture,
            fixture::LOAD_LAYOUT,
            WPARAM(selection),
            LPARAM(0),
        )?;
        ensure!(
            fixture_layout(session.fixture).is_some_and(|active| active.0.addr() == loaded),
            "fixture did not acknowledge the requested active layout"
        );
        session.guard()
    }

    /// Has the fixture reactivate its original layout and unload only the layouts it added, then
    /// checks the fixture layout, the loaded list and the registry keys against the capture. Every
    /// check runs even after a failed capture, load or acknowledgement, and none repairs
    /// anything. Sends no input, so it's allowed before Open and after an abort.
    pub(crate) fn restore(&self, session: &Session) -> Result<()> {
        let restored = request(
            session.fixture,
            fixture::RESTORE_LAYOUTS,
            WPARAM(0),
            LPARAM(0),
        );
        let fixture = match (self.fixture, fixture_layout(session.fixture)) {
            (Some(before), Some(after)) if before == after => Ok(()),
            (Some(_), Some(_)) => Err(anyhow::anyhow!("fixture original layout was not restored")),
            (None, _) => Err(anyhow::anyhow!("the fixture layout was never captured")),
            (Some(_), None) => Err(anyhow::anyhow!(
                "the fixture is gone; its exit restores its layouts"
            )),
        };
        let list = self
            .loaded
            .as_ref()
            .map_err(|error| anyhow::anyhow!("loaded layouts were never captured: {error:#}"))
            .and_then(|before| {
                let after = loaded()?;
                ensure!(
                    after.len() == before.len() && before.iter().all(|layout| after.contains(layout)),
                    "loaded keyboard layout list differs from its snapshot; external changes were not undone"
                );
                Ok(())
            });
        let registry = self.registry.ensure_unchanged(&Registry::snapshot());
        let errors = [
            restored.map(|_| ()).context("fixture layout restore"),
            fixture,
            list,
            registry,
        ]
        .into_iter()
        .filter_map(Result::err)
        .map(|error| format!("{error:#}"))
        .collect::<Vec<_>>();
        if !errors.is_empty() {
            bail!("layout cleanup: {}", errors.join("; "));
        }
        Ok(())
    }
}
