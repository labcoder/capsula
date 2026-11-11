//! Native input surfaces share one order, including the dock paint and drawing windows.
//! A macOS adapter must bind each recreated drawing host before showing it, ignore mouse
//! events over its entire bounds, never activate it, and restore the same layer order after
//! every show or placement with nonactivating `NSPanel`, `ignoresMouseEvents` and levels.

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

// shortcut: slice 5 and B3b bind regional and selection surfaces.
#[expect(dead_code, reason = "slice 5 and B3b bind additional surfaces")]
impl OverlayWindows {
    pub(crate) fn proxy(&mut self, proxy: super::observer::ProxyHandle) {
        self.layers.entry(Layer::Dock).or_default().push(proxy.0);
        self.applied = None;
    }

    pub(crate) fn bind(&mut self, layer: Layer, window: &Window) -> Result<(), PresenceError> {
        self.bind_hwnd(layer, hwnd_of(window)?)
    }

    #[cfg(test)]
    pub(crate) fn check_drawing(dock: &Window, drawing: &Window) -> Result<(), PresenceError> {
        native_tests::drawing(hwnd_of(dock)?, hwnd_of(drawing)?);
        Ok(())
    }

    fn bind_hwnd(&mut self, layer: Layer, hwnd: HWND) -> Result<(), PresenceError> {
        if layer == Layer::Dock {
            interactive(hwnd, false)?;
        }
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
        configure_tracked(&mut self.applied, mode, regions, |styles| {
            configure(&self.layers, &self.regions, mode, regions, styles)
        })
    }
}

/// Whether a configure also rewrites the hit-test styles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Styles {
    Apply,
    Keep,
}

/// Restacks every time, because placing or showing a presence window lifts it to the top of
/// the topmost band; only the style changes are skipped when nothing changed since the last
/// successful apply.
fn configure_tracked<E>(
    applied: &mut Option<(OverlayInput, Regions)>,
    mode: OverlayInput,
    regions: &Regions,
    configure: impl FnOnce(Styles) -> Result<(), E>,
) -> Result<(), E> {
    let unchanged = applied
        .as_ref()
        .is_some_and(|(old_mode, old_regions)| *old_mode == mode && old_regions == regions);
    if unchanged {
        return configure(Styles::Keep);
    }
    *applied = None;
    configure(Styles::Apply)?;
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
    styles: Styles,
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
                if styles == Styles::Apply {
                    interactive(*hwnd, mode == OverlayInput::Regions && rect.is_some())?;
                }
                restack(*hwnd, rect)?;
            }
        }
        for hwnd in layers.get(&layer).into_iter().flatten() {
            if !owned(*hwnd) {
                continue;
            }
            if styles == Styles::Apply && matches!(layer, Layer::Overlay | Layer::Selection) {
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
#[path = "overlay_input_native_tests.rs"]
mod native_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_configuration_still_restacks_and_skips_only_styles() {
        use Styles::{Apply, Keep};
        let mut applied = None;
        let mut styles = Vec::new();
        let empty = Regions::new();
        let registered = Regions::from([(RegionId(1), [10, 20, 4, 5])]);
        let mut apply = |applied: &mut _, mode, regions: &_, result: Result<(), &'static str>| {
            configure_tracked(applied, mode, regions, |changed| {
                styles.push(changed);
                result
            })
        };
        assert_eq!(
            apply(&mut applied, OverlayInput::PassThrough, &empty, Ok(())),
            Ok(())
        );
        // A placement lifted a window above the overlay; the unchanged configure restacks.
        assert_eq!(
            apply(&mut applied, OverlayInput::PassThrough, &empty, Ok(())),
            Ok(())
        );
        assert_eq!(
            apply(&mut applied, OverlayInput::Regions, &registered, Ok(())),
            Ok(())
        );
        assert_eq!(
            apply(&mut applied, OverlayInput::Regions, &empty, Ok(())),
            Ok(())
        );
        assert_eq!(
            apply(
                &mut applied,
                OverlayInput::Selecting,
                &empty,
                Err("partial setup")
            ),
            Err("partial setup")
        );
        assert_eq!(applied, None);
        assert_eq!(
            apply(&mut applied, OverlayInput::Regions, &empty, Ok(())),
            Ok(())
        );
        assert_eq!(styles, vec![Apply, Keep, Apply, Apply, Apply, Apply]);
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
