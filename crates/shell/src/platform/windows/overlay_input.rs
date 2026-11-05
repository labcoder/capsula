//! Native input surfaces; macOS uses nonactivating `NSPanel`, `ignoresMouseEvents` and levels.

use std::collections::BTreeMap;

use gpui_kit::Window;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetWindowLongPtrW, HWND_TOPMOST, IsWindow, LWA_ALPHA, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TRANSPARENT,
};

use super::presence::{PresenceError, hwnd_of};
use crate::presenter::overlay_input::{InputPlatform, Layer, OverlayInput, RegionId, Regions};

/// UI-thread bindings; detach a region before its window closes, then remove its registration.
#[derive(Default)]
pub(crate) struct OverlayWindows {
    layers: BTreeMap<Layer, Vec<HWND>>,
    regions: BTreeMap<RegionId, HWND>,
    applied: Option<(OverlayInput, Regions)>,
}

// shortcut: B5 and B3b bind scene/selection surfaces; slice 5 binds its regional windows.
#[expect(dead_code, reason = "B5, slice 5 and B3b bind additional surfaces")]
impl OverlayWindows {
    pub(crate) fn proxy(&mut self, proxy: super::observer::ProxyHandle) {
        self.layers.entry(Layer::Dock).or_default().push(proxy.0);
        self.applied = None;
    }

    pub(crate) fn bind(&mut self, layer: Layer, window: &Window) -> Result<(), PresenceError> {
        let hwnd = hwnd_of(window)?;
        for windows in self.layers.values_mut() {
            windows.retain(|hwnd| live(*hwnd));
        }
        bind_live(self.layers.entry(layer).or_default(), hwnd, live);
        self.applied = None;
        Ok(())
    }

    pub(crate) fn region(&mut self, id: RegionId, window: &Window) -> Result<(), PresenceError> {
        self.regions.insert(id, hwnd_of(window)?);
        self.applied = None;
        Ok(())
    }

    pub(crate) fn remove_region(&mut self, id: RegionId) {
        self.regions.remove(&id);
        self.applied = None;
    }
}

impl InputPlatform for OverlayWindows {
    type Error = PresenceError;

    fn configure(&mut self, mode: OverlayInput, regions: &Regions) -> Result<(), PresenceError> {
        configure_if_changed(&mut self.applied, mode, regions, || {
            configure(&self.layers, &self.regions, mode, regions)
        })
    }
}

fn configure_if_changed<E>(
    applied: &mut Option<(OverlayInput, Regions)>,
    mode: OverlayInput,
    regions: &Regions,
    configure: impl FnOnce() -> Result<(), E>,
) -> Result<(), E> {
    if applied
        .as_ref()
        .is_some_and(|(old_mode, old_regions)| *old_mode == mode && old_regions == regions)
    {
        return Ok(());
    }
    *applied = None;
    configure()?;
    *applied = Some((mode, regions.clone()));
    Ok(())
}

fn bind_live<T: Copy + PartialEq>(
    windows: &mut Vec<T>,
    window: T,
    mut live: impl FnMut(T) -> bool,
) {
    windows.retain(|hwnd| live(*hwnd));
    if !windows.contains(&window) && live(window) {
        windows.push(window);
    }
}

fn live(hwnd: HWND) -> bool {
    // SAFETY: IsWindow only checks the handle; it never dereferences or changes the window.
    unsafe { IsWindow(Some(hwnd)).as_bool() }
}

fn configure(
    layers: &BTreeMap<Layer, Vec<HWND>>,
    bindings: &BTreeMap<RegionId, HWND>,
    mode: OverlayInput,
    regions: &Regions,
) -> Result<(), PresenceError> {
    if mode == OverlayInput::Selecting
        && layers
            .get(&Layer::Selection)
            .is_none_or(|windows| !windows.iter().copied().any(owned))
    {
        return Err(PresenceError::SelectionSurface);
    }
    for layer in mode.order().into_iter().rev() {
        if layer == Layer::Regions {
            for (id, hwnd) in bindings {
                if !owned(*hwnd) {
                    continue;
                }
                let rect = regions.get(id).copied();
                interactive(*hwnd, mode == OverlayInput::Regions && rect.is_some())?;
                restack(*hwnd, rect)?;
            }
        }
        for hwnd in layers.get(&layer).into_iter().flatten() {
            if !owned(*hwnd) {
                continue;
            }
            if matches!(layer, Layer::Overlay | Layer::Selection) {
                interactive(
                    *hwnd,
                    layer == Layer::Selection && mode == OverlayInput::Selecting,
                )?;
            }
            restack(*hwnd, None)?;
        }
    }
    Ok(())
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "one native hit-test toggle, not independent options"
)]
fn interactive(hwnd: HWND, enabled: bool) -> Result<(), PresenceError> {
    let flag = |value: u32| isize::try_from(value).expect("window flags fit in isize");
    // SAFETY: bindings name live windows owned by this UI thread.
    let old = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    let style = old | flag(WS_EX_NOACTIVATE.0 | WS_EX_LAYERED.0);
    let style = if enabled {
        style & !flag(WS_EX_TRANSPARENT.0)
    } else {
        style | flag(WS_EX_TRANSPARENT.0)
    };
    // SAFETY: changes only extended style flags on this thread's window.
    unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style) };
    // SAFETY: full opacity preserves GPUI's alpha on this live layered window.
    unsafe {
        SetLayeredWindowAttributes(
            hwnd,
            windows::Win32::Foundation::COLORREF(0),
            255,
            LWA_ALPHA,
        )
    }?;
    Ok(())
}

fn owned(hwnd: HWND) -> bool {
    // SAFETY: reads the owner thread; a destroyed handle returns zero.
    let owner =
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(hwnd, None) };
    // SAFETY: reads this UI thread's id without changing state.
    owner == unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }
}

fn restack(hwnd: HWND, rect: Option<[i32; 4]>) -> Result<(), PresenceError> {
    let [x, y, width, height] = rect.unwrap_or([0; 4]);
    let flags = SWP_NOACTIVATE
        | if rect.is_some() {
            windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS::default()
        } else {
            SWP_NOMOVE | SWP_NOSIZE
        };
    // SAFETY: restacks only a bound UI-thread surface, without showing or activating it.
    unsafe { SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, width, height, flags) }?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_configuration_skips_native_work_and_failed_changes_retry() {
        let mut applied = None;
        let mut calls = 0;
        let empty = Regions::new();
        let registered = Regions::from([(RegionId(1), [10, 20, 4, 5])]);
        let mut apply = |applied: &mut _, mode, regions: &_| {
            configure_if_changed(applied, mode, regions, || {
                calls += 1;
                Ok::<_, &str>(())
            })
        };
        assert_eq!(
            apply(&mut applied, OverlayInput::PassThrough, &empty),
            Ok(())
        );
        assert_eq!(
            apply(&mut applied, OverlayInput::PassThrough, &empty),
            Ok(())
        );
        assert_eq!(calls, 1);
        assert_eq!(
            configure_if_changed(&mut applied, OverlayInput::Regions, &registered, || {
                calls += 1;
                Ok::<_, &str>(())
            }),
            Ok(())
        );
        assert_eq!(calls, 2);
        assert_eq!(
            configure_if_changed(&mut applied, OverlayInput::Regions, &empty, || {
                calls += 1;
                Ok::<_, &str>(())
            }),
            Ok(())
        );
        assert_eq!(calls, 3);
        assert_eq!(
            configure_if_changed(&mut applied, OverlayInput::Selecting, &empty, || {
                calls += 1;
                Err("partial setup")
            }),
            Err("partial setup")
        );
        assert_eq!(applied, None);
        assert_eq!(
            configure_if_changed(&mut applied, OverlayInput::Regions, &empty, || {
                calls += 1;
                Ok::<_, &str>(())
            }),
            Ok(())
        );
        assert_eq!(calls, 5);
        assert_eq!(applied, Some((OverlayInput::Regions, empty)));
    }

    #[test]
    fn binding_prunes_dead_handles_and_does_not_duplicate_live_handles() {
        let mut windows = vec![10, 20];
        bind_live(&mut windows, 20, |handle| [20, 30].contains(&handle));
        assert_eq!(windows, vec![20]);
        bind_live(&mut windows, 30, |handle| [20, 30].contains(&handle));
        assert_eq!(windows, vec![20, 30]);
        bind_live(&mut windows, 40, |handle| [20, 30].contains(&handle));
        assert_eq!(windows, vec![20, 30]);
        bind_live(&mut windows, 10, |handle| [10, 30].contains(&handle));
        assert_eq!(windows, vec![30, 10]);
    }
}
