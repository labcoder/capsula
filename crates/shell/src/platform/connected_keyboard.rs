//! The gesture dispatch contract; only the Windows callback can attach a native layout.

use std::time::Instant;

use crate::input::{Event, TalkHeld};
#[cfg(windows)]
use windows::Win32::UI::Input::KeyboardAndMouse::HKL;

/// Which check refused to re-arm a blocked shortcut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The event came with no exact key-state read.
    Unread,
    /// A key other than the chord's was down, or a chord key wasn't.
    #[cfg(windows)]
    Chord,
    /// The foreground changed during the read.
    #[cfg(windows)]
    Foreground,
    /// The event's layout is no longer the foreground's.
    Layout,
}

#[cfg(any(test, feature = "gate-recorder"))]
impl Refusal {
    /// A gate run's diagnostic line for this refusal; it names no key.
    pub(crate) fn line(self) -> &'static str {
        match self {
            Self::Unread => "rearm refused check=unread",
            #[cfg(windows)]
            Self::Chord => "rearm refused check=chord",
            #[cfg(windows)]
            Self::Foreground => "rearm refused check=foreground",
            Self::Layout => "rearm refused check=layout",
        }
    }
}

/// When the native callback that carried an event began, for gesture timing.
#[derive(Clone, Copy)]
pub(crate) struct EventTime {
    pub(crate) instant: Instant,
    pub(crate) held: TalkHeld,
    pub(crate) repeated_modifier: bool,
    pub(crate) defer_terminal: bool,
    pub(crate) queued_press: Option<u64>,
    pub(crate) speech_escape_owner: crate::escape::SpeechEscapeOwner,
    pub(super) read_chord: Option<fn(Instant) -> Result<Instant, Refusal>>,
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
            queued_press: None,
            speech_escape_owner: crate::escape::SpeechEscapeOwner::Outside,
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

    pub(crate) fn recovery_time(self) -> Result<Instant, Refusal> {
        let now = self.read_chord.ok_or(Refusal::Unread)?(self.instant)?;
        if self.layout_current() {
            Ok(now)
        } else {
            Err(Refusal::Layout)
        }
    }

    #[cfg(test)]
    pub(crate) fn with_keyboard_read(
        mut self,
        read: fn(Instant) -> Result<Instant, Refusal>,
    ) -> Self {
        self.read_chord = Some(read);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_queued_press(mut self, at: u64) -> Self {
        self.queued_press = Some(at);
        self
    }
}

/// Reduces one gesture event at its callback time and returns the next deadline to arm.
pub(crate) type Deliver = Box<dyn Fn(Event, EventTime) -> Option<Instant>>;

#[cfg(all(test, windows))]
mod tests {
    use std::time::Instant;

    use super::{EventTime, HKL, Refusal};

    #[test]
    fn a_refused_rearm_names_the_check_that_refused() {
        let at = EventTime::new(Instant::now());
        assert_eq!(at.recovery_time(), Err(Refusal::Unread));
        assert_eq!(
            at.with_keyboard_read(|_| Err(Refusal::Chord))
                .recovery_time(),
            Err(Refusal::Chord)
        );
        let mut read = at.with_keyboard_read(Ok);
        assert_eq!(read.recovery_time(), Ok(at.instant));
        read.layout = Some(HKL(std::ptr::without_provenance_mut(usize::MAX)));
        assert_eq!(read.recovery_time(), Err(Refusal::Layout));
    }
}
