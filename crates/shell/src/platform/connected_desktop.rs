//! A host without the connected desktop backend cannot own any native bindings.

#[cfg(test)]
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

use async_channel::Sender;
use raw_window_handle::HasWindowHandle;

#[cfg(test)]
use kumochi_shell::presence_platform as geometry;

use super::host::{HostFacts, HostId, WatchGen};
use super::{
    Deliver, MaskUpdate, Observe, PresenceError, PresenceEvent, PresenceLabels, PresenceSurface,
};
use crate::presenter::overlay_input::{InputPlatform, Layer, OverlayInput, Regions};

#[cfg_attr(test, derive(Default))]
pub(crate) enum OverlayWindows {
    #[cfg(test)]
    #[default]
    Detached,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Foreground {}

#[derive(Debug)]
pub(crate) enum HostWatch {}

pub(crate) enum PresenceNative {
    #[cfg(test)]
    Detached {
        talk_escape: Rc<Cell<bool>>,
        refuse_talk_escape: Cell<bool>,
    },
}

#[cfg(not(test))]
pub(crate) enum EscapeHotkey {}

#[cfg(test)]
pub(crate) struct EscapeHotkey(Rc<Cell<bool>>);

#[cfg(test)]
impl Drop for EscapeHotkey {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProxyHandle(std::convert::Infallible);

pub(crate) enum Affinity {}

#[cfg(test)]
type FixtureCursor = Cell<Option<[i32; 2]>>;

#[cfg(test)]
thread_local! {
    static DETACHED_CURSOR: RefCell<Weak<FixtureCursor>> = const { RefCell::new(Weak::new()) };
}

#[cfg(test)]
const DEFAULT_CURSOR: [i32; 2] = [4096, 4096];

#[cfg(test)]
pub(crate) struct DetachedInputs {
    _cursor: Rc<FixtureCursor>,
}

#[cfg(test)]
impl DetachedInputs {
    pub(crate) fn new() -> Self {
        let cursor = DETACHED_CURSOR.with_borrow_mut(|slot| {
            slot.upgrade().unwrap_or_else(|| {
                let cursor = Rc::new(Cell::new(Some(DEFAULT_CURSOR)));
                *slot = Rc::downgrade(&cursor);
                cursor
            })
        });
        Self { _cursor: cursor }
    }

    pub(super) fn active() -> bool {
        DETACHED_CURSOR.with_borrow(|slot| slot.strong_count() != 0)
    }

    pub(super) fn set_cursor(at: Option<[i32; 2]>) {
        DETACHED_CURSOR
            .with_borrow(Weak::upgrade)
            .expect("the test owns detached input")
            .set(at);
    }
}

pub(crate) fn cursor_position() -> Result<[i32; 2], PresenceError> {
    #[cfg(test)]
    if let Some(point) = super::cursor_fixture::get().or_else(|| {
        DETACHED_CURSOR
            .with_borrow(Weak::upgrade)
            .map(|cursor| cursor.get().ok_or(()))
    }) {
        return point.map_err(|()| PresenceError::BindingsUnavailable);
    }
    Err(PresenceError::BindingsUnavailable)
}

pub(crate) fn host_at(_: [i32; 2]) -> Option<HostFacts> {
    None
}

pub(crate) fn watch_host(
    _: HostId,
    _: WatchGen,
    _: Sender<PresenceEvent>,
) -> Result<HostWatch, PresenceError> {
    Err(PresenceError::BindingsUnavailable)
}

pub(crate) fn window_handle(_: &impl HasWindowHandle) -> Result<usize, PresenceError> {
    Err(PresenceError::BindingsUnavailable)
}

pub(crate) fn window_affinity(_: &impl HasWindowHandle) -> Result<Affinity, PresenceError> {
    Err(PresenceError::BindingsUnavailable)
}

pub(crate) fn exclude_window(
    _: &impl HasWindowHandle,
    prior: &Affinity,
) -> Result<(), PresenceError> {
    match *prior {}
}

pub(crate) fn restore_window_affinity(
    _: &impl HasWindowHandle,
    prior: &Affinity,
) -> Result<(), PresenceError> {
    match *prior {}
}

#[cfg(test)]
fn detached_window(window: &impl HasWindowHandle) -> bool {
    DetachedInputs::active()
        && matches!(
            window.window_handle(),
            Err(raw_window_handle::HandleError::NotSupported)
        )
}

#[cfg(test)]
pub(crate) fn place_presence_window(
    window: &mut impl HasWindowHandle,
    bounds: [i32; 4],
) -> Result<(), PresenceError> {
    if detached_window(window) {
        return Ok(());
    }
    geometry::place_presence_window(window, bounds)
}

#[cfg(test)]
pub(crate) fn hide_presence_window(window: &mut impl HasWindowHandle) -> Result<(), PresenceError> {
    if detached_window(window) {
        return Ok(());
    }
    geometry::hide_presence_window(window)
}

#[cfg(test)]
pub(crate) fn work_area(_: &gpui_kit::App) -> Result<[i32; 4], PresenceError> {
    super::work_area_fixture::get()
        .or(DetachedInputs::active().then_some([0, 0, 2560, 1400]))
        .map_or_else(geometry::work_area, Ok)
}

#[cfg(test)]
pub(crate) fn client_origin(window: &impl HasWindowHandle) -> Result<[i32; 2], PresenceError> {
    if detached_window(window) {
        return Ok([0, 0]);
    }
    geometry::client_origin(window)
}

impl Affinity {
    pub(crate) fn gone(&self) -> bool {
        match *self {}
    }
}

impl ProxyHandle {
    pub(crate) fn value(self) -> usize {
        match self.0 {}
    }

    pub(crate) fn post_drawn(self, _: u64) -> Result<(), PresenceError> {
        match self.0 {}
    }
}

impl PresenceNative {
    #[cfg(test)]
    pub(crate) fn detached() -> Self {
        Self::Detached {
            talk_escape: Rc::default(),
            refuse_talk_escape: Cell::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn refuse_talk_escape_for_test(&self) {
        let Self::Detached {
            refuse_talk_escape, ..
        } = self;
        refuse_talk_escape.set(true);
    }

    pub(crate) fn new(
        _: PresenceLabels,
        _: Sender<PresenceEvent>,
        _: [i32; 2],
    ) -> Result<Self, PresenceError> {
        Err(PresenceError::BindingsUnavailable)
    }

    pub(crate) fn reshape(
        &mut self,
        _: [i32; 4],
        _: impl Fn(i32) -> crate::presence::line::machine::RowSpans,
    ) -> Result<MaskUpdate, PresenceError> {
        self.unavailable()
    }

    pub(crate) fn show(&mut self, _: usize) -> Result<(), PresenceError> {
        self.unavailable()
    }

    pub(crate) fn hide(&mut self) -> Result<(), PresenceError> {
        self.unavailable()
    }

    pub(crate) fn observe(&self, _: Observe) -> ProxyHandle {
        match *self {
            #[cfg(test)]
            Self::Detached { .. } => panic!("the detached fixture has no native proxy"),
        }
    }

    pub(crate) fn arm_keyboard(&mut self, _: Deliver) -> Result<(), PresenceError> {
        self.unavailable()
    }

    pub(crate) fn register_escape(&self) -> Result<EscapeHotkey, PresenceError> {
        self.unavailable()
    }

    pub(crate) fn register_talk_escape(&self) -> Result<EscapeHotkey, PresenceError> {
        match *self {
            #[cfg(test)]
            Self::Detached {
                ref talk_escape,
                ref refuse_talk_escape,
            } => {
                if refuse_talk_escape.get() || talk_escape.replace(true) {
                    return Err(PresenceError::BindingsUnavailable);
                }
                Ok(EscapeHotkey(Rc::clone(talk_escape)))
            }
        }
    }

    pub(crate) fn escape_trusted(&self) -> bool {
        match *self {
            #[cfg(test)]
            Self::Detached { .. } => true,
        }
    }

    pub(crate) fn enable_retry(&self) {
        match *self {
            #[cfg(test)]
            Self::Detached { .. } => (),
        }
    }

    pub(crate) fn disable_retry(&self) {
        match *self {
            #[cfg(test)]
            Self::Detached { .. } => (),
        }
    }

    pub(crate) fn set_taskbar_button(
        &mut self,
        _: crate::settings::TaskbarChoice,
    ) -> Result<(), PresenceError> {
        self.unavailable()
    }

    pub(crate) fn affinity(&self) -> Result<Affinity, PresenceError> {
        self.unavailable()
    }

    fn unavailable<T>(&self) -> Result<T, PresenceError> {
        match *self {
            #[cfg(test)]
            Self::Detached { .. } => Err(PresenceError::BindingsUnavailable),
        }
    }

    pub(crate) fn exclude(&self, prior: &Affinity) -> Result<(), PresenceError> {
        let _ = prior;
        match *self {
            #[cfg(test)]
            Self::Detached { .. } => match *prior {},
        }
    }

    pub(crate) fn restore_affinity(&self, prior: &Affinity) -> Result<(), PresenceError> {
        let _ = prior;
        match *self {
            #[cfg(test)]
            Self::Detached { .. } => match *prior {},
        }
    }
}

pub(crate) fn configure_presence_window(
    _: &mut impl HasWindowHandle,
    _: PresenceSurface,
    _: Sender<PresenceEvent>,
) -> Result<(), PresenceError> {
    Err(PresenceError::BindingsUnavailable)
}

pub(crate) fn activate_presence_window(
    window: &impl HasWindowHandle,
) -> Result<Option<Foreground>, PresenceError> {
    #[cfg(test)]
    if detached_window(window) {
        return Ok(None);
    }
    let _ = window;
    Err(PresenceError::ForegroundUnavailable)
}

pub(crate) fn restore_foreground(_: &impl HasWindowHandle, previous: Foreground) {
    match previous {}
}

impl OverlayWindows {
    pub(crate) fn bind_proxy(&mut self, _: &PresenceNative) {
        match *self {
            #[cfg(test)]
            Self::Detached => (),
        }
    }

    pub(crate) fn bind(&mut self, _: Layer, _: &impl HasWindowHandle) -> Result<(), PresenceError> {
        match *self {
            #[cfg(test)]
            Self::Detached => Err(PresenceError::BindingsUnavailable),
        }
    }
}

impl InputPlatform for OverlayWindows {
    type Error = PresenceError;

    fn configure(&mut self, mode: OverlayInput, _: &Regions) -> Result<(), PresenceError> {
        let _ = mode;
        match *self {
            #[cfg(test)]
            Self::Detached if mode == OverlayInput::Selecting => {
                Err(PresenceError::SelectionSurface)
            }
            #[cfg(test)]
            Self::Detached => Ok(()),
        }
    }
}
