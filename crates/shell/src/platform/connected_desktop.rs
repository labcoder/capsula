//! A host without the connected desktop backend cannot own any native bindings.

use async_channel::Sender;
use raw_window_handle::HasWindowHandle;

use super::host::{HostFacts, HostId, WatchGen};
use super::{
    Deliver, MaskUpdate, Observe, PresenceError, PresenceEvent, PresenceLabels, PresenceSurface,
};
use crate::presenter::overlay_input::{InputPlatform, Layer, OverlayInput, Regions};

pub(crate) enum OverlayWindows {}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Foreground {}

#[derive(Debug)]
pub(crate) enum HostWatch {}

pub(crate) enum PresenceNative {}

pub(crate) enum EscapeHotkey {}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProxyHandle(std::convert::Infallible);

pub(crate) enum Affinity {}

pub(crate) fn cursor_position() -> Result<[i32; 2], PresenceError> {
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
        match *self {}
    }

    pub(crate) fn show(&mut self, _: usize) -> Result<(), PresenceError> {
        match *self {}
    }

    pub(crate) fn hide(&mut self) -> Result<(), PresenceError> {
        match *self {}
    }

    pub(crate) fn observe(&self, _: Observe) -> ProxyHandle {
        match *self {}
    }

    pub(crate) fn arm_keyboard(&mut self, _: Deliver) -> Result<(), PresenceError> {
        match *self {}
    }

    pub(crate) fn register_escape(&self) -> Result<EscapeHotkey, PresenceError> {
        match *self {}
    }

    pub(crate) fn register_talk_escape(&self) -> Result<EscapeHotkey, PresenceError> {
        match *self {}
    }

    pub(crate) fn escape_trusted(&self) -> bool {
        match *self {}
    }

    pub(crate) fn enable_retry(&self) {
        match *self {}
    }

    pub(crate) fn disable_retry(&self) {
        match *self {}
    }

    pub(crate) fn set_taskbar_button(
        &mut self,
        _: crate::settings::TaskbarChoice,
    ) -> Result<(), PresenceError> {
        match *self {}
    }

    pub(crate) fn affinity(&self) -> Result<Affinity, PresenceError> {
        match *self {}
    }

    pub(crate) fn exclude(&self, _: &Affinity) -> Result<(), PresenceError> {
        match *self {}
    }

    pub(crate) fn restore_affinity(&self, _: &Affinity) -> Result<(), PresenceError> {
        match *self {}
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
    _: &impl HasWindowHandle,
) -> Result<Option<Foreground>, PresenceError> {
    Err(PresenceError::ForegroundUnavailable)
}

pub(crate) fn restore_foreground(_: &impl HasWindowHandle, previous: Foreground) {
    match previous {}
}

impl OverlayWindows {
    pub(crate) fn bind_proxy(&mut self, _: &PresenceNative) {
        match *self {}
    }

    pub(crate) fn bind(&mut self, _: Layer, _: &impl HasWindowHandle) -> Result<(), PresenceError> {
        match *self {}
    }
}

impl InputPlatform for OverlayWindows {
    type Error = PresenceError;

    fn configure(&mut self, _: OverlayInput, _: &Regions) -> Result<(), PresenceError> {
        match *self {}
    }
}
