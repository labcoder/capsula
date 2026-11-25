//! The gesture dispatch contract; only the Windows callback can attach a native layout.

use std::time::Instant;

use crate::input::{Event, TalkHeld};
#[cfg(windows)]
use windows::Win32::UI::Input::KeyboardAndMouse::HKL;

/// When the native callback that carried an event began, for gesture timing.
#[derive(Clone, Copy)]
pub(crate) struct EventTime {
    pub(crate) instant: Instant,
    pub(crate) held: TalkHeld,
    pub(crate) repeated_modifier: bool,
    pub(crate) defer_terminal: bool,
    pub(super) read_chord: Option<fn(Instant) -> Option<Instant>>,
    #[cfg(windows)]
    pub(super) layout: Option<HKL>,
}

impl EventTime {
    pub(crate) fn new(instant: Instant) -> Self {
        Self {
            instant,
            held: TalkHeld::default(),
            repeated_modifier: false,
            defer_terminal: false,
            read_chord: None,
            #[cfg(windows)]
            layout: None,
        }
    }

    #[cfg(windows)]
    pub(crate) fn layout_current(self) -> bool {
        self.layout
            .is_none_or(|layout| super::windows::foreground_layout() == Some(layout))
    }

    #[cfg(not(windows))]
    pub(crate) fn layout_current(self) -> bool {
        let _ = self;
        #[cfg(test)]
        {
            super::DetachedInputs::active()
        }
        #[cfg(not(test))]
        false
    }

    pub(crate) fn recovery_time(self) -> Option<Instant> {
        let now = self.read_chord?(self.instant)?;
        self.layout_current().then_some(now)
    }

    #[cfg(test)]
    pub(crate) fn with_keyboard_read(mut self, read: fn(Instant) -> Option<Instant>) -> Self {
        self.read_chord = Some(read);
        self
    }
}

/// Reduces one gesture event at its callback time and returns the next deadline to arm.
pub(crate) type Deliver = Box<dyn Fn(Event, EventTime) -> Option<Instant>>;
