//! Left Ctrl+Alt gestures, independent of native event delivery and the UI.
//!
//! A tap runs from chord make to its first modifier release. Two completed short taps
//! open input when their releases are less than 300ms apart. Either modifier may stay
//! held between taps. After another key cancels, all tracked keys must release to rearm.
//! A second tap that misses the window becomes a new first tap if it is still short.
//! Holding starts at 300ms inclusive, even when the release arrives before its timer.
//! Callers deliver monotonic timestamps and schedule only the next returned deadline.

use std::time::{Duration, Instant};

pub(crate) mod hold;

const WINDOW: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    LeftControl,
    LeftAlt,
    /// Native make code: low eight bits, E0 in bit eight and E1 in bit nine.
    Other(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Edge {
    Down,
    Up,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    Key { key: Key, edge: Edge },
    Deadline,
    Reset,
    Escape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    Listening,
    Resting,
    HoldStarted,
    HoldEnded,
    OpenInput,
}

#[derive(Debug, Default, Clone, Copy)]
enum Phase {
    #[default]
    Ready,
    Active {
        started: Instant,
        holding: bool,
    },
    Releasing,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Gesture {
    down: [bool; 2],
    other: [u64; 16],
    phase: Phase,
    first_tap: Option<Instant>,
    legacy_hold: bool,
}

impl Gesture {
    #[cfg(test)]
    pub(crate) fn legacy_hold_fixture() -> Self {
        Self {
            legacy_hold: true,
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub(crate) fn holding(&self) -> bool {
        self.legacy_hold && matches!(self.phase, Phase::Active { holding: true, .. })
    }

    pub(crate) fn listening(&self) -> bool {
        self.legacy_hold && matches!(self.phase, Phase::Active { .. })
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        let hold = match self.phase {
            Phase::Active {
                started,
                holding: false,
            } => Some(started + WINDOW),
            _ => None,
        };
        match (hold, self.first_tap.map(|released| released + WINDOW)) {
            (Some(hold), Some(tap)) => Some(hold.min(tap)),
            (hold, tap) => hold.or(tap),
        }
    }

    #[cfg(test)]
    pub(crate) fn handle(&mut self, event: Event, now: Instant) -> [Option<Effect>; 3] {
        let effects = self.recognize(event, now);
        if self.legacy_hold {
            effects
        } else {
            effects.map(|effect| effect.filter(|effect| *effect == Effect::OpenInput))
        }
    }

    fn reset(&mut self) {
        *self = Self {
            legacy_hold: self.legacy_hold,
            ..Self::default()
        };
    }

    fn recognize(&mut self, event: Event, now: Instant) -> [Option<Effect>; 3] {
        if matches!(event, Event::Reset | Event::Escape) {
            if !matches!(self.phase, Phase::Ready) {
                tracing::debug!(stage = ?hold::InputStage::Gesture, reason = ?hold::InputReason::Reset, "held speech input boundary");
            }
            let effects = self.cancel();
            self.reset();
            return effects;
        }
        let Event::Key { key, edge } = event else {
            return [self.advance(now), None, None];
        };
        let index = match key {
            Key::LeftControl => 0,
            Key::LeftAlt => 1,
            Key::Other(code) => return self.other_key(code, edge),
        };
        let down = edge == Edge::Down;
        // shortcut: a held key's auto-repeat also reaches the threshold when the deadline timer is
        // late, but only after the keyboard's repeat delay (about 500 ms, up to 1 s), so such a
        // hold starts late and one released sooner is refused; upgrade once the deadline can't starve.
        if self.down[index] == down {
            return [self.advance(now), None, None];
        }
        let threshold = self.advance(now);
        self.down[index] = down;
        if down && self.down == [true; 2] {
            let reason = if matches!(self.phase, Phase::Ready) && self.other == [0; 16] {
                hold::InputReason::ChordStarted
            } else {
                hold::InputReason::ChordSuppressed
            };
            tracing::debug!(stage = ?hold::InputStage::Gesture, ?reason, "held speech input boundary");
        }
        if down
            && matches!(self.phase, Phase::Ready)
            && self.down == [true; 2]
            && self.other == [0; 16]
        {
            self.phase = Phase::Active {
                started: now,
                holding: false,
            };
            return [Some(Effect::Listening), None, None];
        }
        if !down {
            if let Phase::Active { holding, .. } = self.phase {
                self.phase = Phase::Ready;
                if holding {
                    return [threshold, Some(Effect::HoldEnded), Some(Effect::Resting)];
                }
                let open = self.first_tap.take().map(|_| Effect::OpenInput);
                if open.is_none() {
                    self.first_tap = Some(now);
                }
                return [Some(Effect::Resting), open, None];
            }
            if self.down == [false; 2] && self.other == [0; 16] {
                self.phase = Phase::Ready;
            }
        }
        [threshold, None, None]
    }

    fn cancel(&mut self) -> [Option<Effect>; 3] {
        let effects = match self.phase {
            Phase::Active { holding: true, .. } => {
                [Some(Effect::HoldEnded), Some(Effect::Resting), None]
            }
            Phase::Active { .. } => [Some(Effect::Resting), None, None],
            _ => [None; 3],
        };
        self.first_tap = None;
        self.phase = Phase::Releasing;
        effects
    }

    fn other_key(&mut self, code: u16, edge: Edge) -> [Option<Effect>; 3] {
        if edge == Edge::Down && matches!(self.phase, Phase::Active { .. }) {
            tracing::debug!(stage = ?hold::InputStage::Gesture, reason = ?hold::InputReason::Interrupted, "held speech input boundary");
        }
        if code >= 1024 {
            let effects = self.cancel();
            self.reset();
            return effects;
        }
        let word = usize::from(code / 64);
        let mask = 1u64 << (code % 64);
        if edge == Edge::Down {
            if self.other[word] & mask != 0 {
                return [None; 3];
            }
            self.other[word] |= mask;
            return self.cancel();
        }
        self.other[word] &= !mask;
        if self.down == [false; 2] && self.other == [0; 16] {
            self.phase = Phase::Ready;
        }
        [None; 3]
    }

    fn advance(&mut self, now: Instant) -> Option<Effect> {
        if self
            .first_tap
            .is_some_and(|released| now >= released + WINDOW)
        {
            self.first_tap = None;
        }
        if let Phase::Active {
            started,
            holding: false,
        } = self.phase
            && now >= started + WINDOW
        {
            self.phase = Phase::Active {
                started,
                holding: true,
            };
            self.first_tap = None;
            return Some(Effect::HoldStarted);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{Edge, Effect, Event, Gesture, Key, WINDOW};
    use std::time::{Duration, Instant};

    #[test]
    fn production_holds_are_silent_but_double_taps_still_open_typing() {
        let mut gesture = Gesture::default();
        let now = Instant::now();
        assert_eq!(
            key(&mut gesture, Key::LeftControl, Edge::Down, now),
            [None; 3]
        );
        assert_eq!(key(&mut gesture, Key::LeftAlt, Edge::Down, now), [None; 3]);
        assert!(!gesture.listening());
        assert_eq!(gesture.handle(Event::Deadline, now + WINDOW), [None; 3]);
        assert!(!gesture.holding());
        assert_eq!(
            key(&mut gesture, Key::LeftAlt, Edge::Up, now + WINDOW),
            [None; 3]
        );
        assert_eq!(
            key(&mut gesture, Key::LeftControl, Edge::Up, now + WINDOW),
            [None; 3]
        );
        let tap = now + WINDOW * 2;
        assert_eq!(
            key(&mut gesture, Key::LeftControl, Edge::Down, tap),
            [None; 3]
        );
        assert_eq!(key(&mut gesture, Key::LeftAlt, Edge::Down, tap), [None; 3]);
        assert_eq!(key(&mut gesture, Key::LeftAlt, Edge::Up, tap), [None; 3]);
        assert_eq!(
            key(
                &mut gesture,
                Key::LeftAlt,
                Edge::Down,
                tap + Duration::from_millis(1)
            ),
            [None; 3]
        );
        assert_eq!(
            key(
                &mut gesture,
                Key::LeftAlt,
                Edge::Up,
                tap + Duration::from_millis(2)
            ),
            [None, Some(Effect::OpenInput), None]
        );
        assert!(!gesture.listening());
        assert!(!gesture.holding());
        assert_eq!(gesture.handle(Event::Reset, tap + WINDOW), [None; 3]);
    }

    fn key(gesture: &mut Gesture, key: Key, edge: Edge, now: Instant) -> [Option<Effect>; 3] {
        gesture.handle(Event::Key { key, edge }, now)
    }

    fn chord(gesture: &mut Gesture, now: Instant) {
        assert_eq!(
            key(gesture, Key::LeftControl, Edge::Down, now),
            [None; 3],
            "first modifier"
        );
        assert_eq!(
            key(gesture, Key::LeftAlt, Edge::Down, now),
            [Some(Effect::Listening), None, None],
            "chord activates"
        );
    }

    #[test]
    fn new_panels_can_inherit_capture_exclusion_only_during_a_recognized_hold() {
        let now = Instant::now();
        let mut gesture = Gesture::legacy_hold_fixture();
        assert!(!gesture.holding());
        chord(&mut gesture, now);
        assert!(
            !gesture.holding(),
            "a short tap doesn't exclude a new panel"
        );
        assert_eq!(
            gesture.handle(Event::Deadline, now + WINDOW),
            [Some(Effect::HoldStarted), None, None]
        );
        assert!(gesture.holding());
        key(&mut gesture, Key::LeftAlt, Edge::Up, now + WINDOW);
        assert!(!gesture.holding(), "release restores future panels too");
    }

    fn tap(gesture: &mut Gesture, now: Instant) {
        chord(gesture, now);
        assert_eq!(
            key(gesture, Key::LeftControl, Edge::Up, now),
            [Some(Effect::Resting), None, None],
            "first tap releases"
        );
        assert_eq!(
            key(gesture, Key::LeftAlt, Edge::Up, now),
            [None; 3],
            "both modifiers release"
        );
    }

    #[test]
    fn either_modifier_and_release_order_works_and_repeats_do_nothing() {
        let now = Instant::now();
        for first in [Key::LeftControl, Key::LeftAlt] {
            for released in [Key::LeftControl, Key::LeftAlt] {
                let second = if first == Key::LeftControl {
                    Key::LeftAlt
                } else {
                    Key::LeftControl
                };
                let mut gesture = Gesture::legacy_hold_fixture();
                assert_eq!(
                    key(&mut gesture, first, Edge::Down, now),
                    [None; 3],
                    "first modifier"
                );
                assert_eq!(
                    key(&mut gesture, first, Edge::Down, now),
                    [None; 3],
                    "repeat before chord"
                );
                assert_eq!(
                    key(&mut gesture, second, Edge::Down, now),
                    [Some(Effect::Listening), None, None],
                    "either order"
                );
                assert!(gesture.listening(), "chord is listening");
                assert_eq!(
                    key(&mut gesture, second, Edge::Down, now),
                    [None; 3],
                    "repeat during chord"
                );
                assert_eq!(
                    key(&mut gesture, released, Edge::Up, now),
                    [Some(Effect::Resting), None, None],
                    "either release ends listening"
                );
                assert!(!gesture.listening(), "first release ends listening");
            }
        }
    }

    #[test]
    fn a_chord_key_repeat_starts_the_hold_once_and_only_from_the_threshold() {
        let start = Instant::now();
        let at = |millis| start + Duration::from_millis(millis);
        let mut gesture = Gesture::legacy_hold_fixture();
        chord(&mut gesture, start);
        assert_eq!(
            key(&mut gesture, Key::LeftAlt, Edge::Down, at(299)),
            [None; 3],
            "a repeat before the threshold"
        );
        assert_eq!(
            key(&mut gesture, Key::LeftAlt, Edge::Down, at(300)),
            [Some(Effect::HoldStarted), None, None],
            "a repeat at the threshold"
        );
        assert!(gesture.holding());
        assert_eq!(
            key(&mut gesture, Key::LeftControl, Edge::Down, at(330)),
            [None; 3],
            "a repeat once holding"
        );
        assert_eq!(
            gesture.handle(Event::Deadline, at(340)),
            [None; 3],
            "the late timer once holding"
        );
        let mut ready = Gesture::legacy_hold_fixture();
        assert_eq!(key(&mut ready, Key::LeftAlt, Edge::Down, start), [None; 3]);
        assert_eq!(
            key(&mut ready, Key::LeftAlt, Edge::Down, at(400)),
            [None; 3],
            "a repeat while ready"
        );
        let mut releasing = Gesture::legacy_hold_fixture();
        chord(&mut releasing, start);
        assert_eq!(
            key(&mut releasing, Key::Other(0x1E), Edge::Down, at(10)),
            [Some(Effect::Resting), None, None],
            "another key cancels"
        );
        assert_eq!(
            key(&mut releasing, Key::LeftAlt, Edge::Down, at(400)),
            [None; 3],
            "a repeat while releasing"
        );
    }

    #[test]
    fn hold_threshold_and_late_timer_delivery_are_exact() {
        let start = Instant::now();
        for milliseconds in [299, 300, 301] {
            let mut gesture = Gesture::legacy_hold_fixture();
            chord(&mut gesture, start);
            assert_eq!(
                gesture.deadline(),
                Some(start + WINDOW),
                "one-shot hold deadline"
            );
            let released = key(
                &mut gesture,
                Key::LeftAlt,
                Edge::Up,
                start + Duration::from_millis(milliseconds),
            );
            let expected = if milliseconds < 300 {
                [Some(Effect::Resting), None, None]
            } else {
                [
                    Some(Effect::HoldStarted),
                    Some(Effect::HoldEnded),
                    Some(Effect::Resting),
                ]
            };
            assert_eq!(
                released, expected,
                "release at {milliseconds}ms without timer delivery"
            );
        }
        let mut gesture = Gesture::legacy_hold_fixture();
        chord(&mut gesture, start);
        assert_eq!(
            gesture.handle(Event::Deadline, start + Duration::from_millis(299)),
            [None; 3],
            "hold does not start before 300ms"
        );
        assert_eq!(
            gesture.handle(Event::Deadline, start + WINDOW),
            [Some(Effect::HoldStarted), None, None],
            "hold starts once"
        );
        assert_eq!(gesture.deadline(), None, "held chord needs no polling");
        assert_eq!(
            gesture.handle(Event::Deadline, start + WINDOW),
            [None; 3],
            "duplicate timer"
        );
        assert_eq!(
            key(&mut gesture, Key::LeftControl, Edge::Up, start + WINDOW),
            [None, Some(Effect::HoldEnded), Some(Effect::Resting)],
            "delivered hold ends"
        );
    }

    #[test]
    fn completed_double_tap_is_strictly_inside_window() {
        let start = Instant::now();
        for milliseconds in [299, 300] {
            let mut gesture = Gesture::legacy_hold_fixture();
            tap(&mut gesture, start);
            chord(&mut gesture, start + Duration::from_millis(100));
            let effects = key(
                &mut gesture,
                Key::LeftAlt,
                Edge::Up,
                start + Duration::from_millis(milliseconds),
            );
            assert_eq!(
                effects,
                [
                    Some(Effect::Resting),
                    (milliseconds == 299).then_some(Effect::OpenInput),
                    None
                ],
                "second release at {milliseconds}ms"
            );
        }
    }

    #[test]
    fn pending_tap_expires_while_second_chord_can_become_a_hold() {
        let start = Instant::now();
        let mut gesture = Gesture::legacy_hold_fixture();
        tap(&mut gesture, start);
        chord(&mut gesture, start + Duration::from_millis(100));
        assert_eq!(
            gesture.deadline(),
            Some(start + WINDOW),
            "tap expiry precedes hold"
        );
        assert_eq!(
            gesture.handle(Event::Deadline, start + WINDOW),
            [None; 3],
            "second press alone never opens input"
        );
        assert_eq!(
            gesture.deadline(),
            Some(start + Duration::from_millis(400)),
            "hold timer remains"
        );
        assert_eq!(
            gesture.handle(Event::Deadline, start + Duration::from_millis(400)),
            [Some(Effect::HoldStarted), None, None],
            "second long press is a hold"
        );
    }

    #[test]
    fn interruptions_cancel_taps_and_require_both_modifiers_up() {
        let start = Instant::now();
        let mut gesture = Gesture::legacy_hold_fixture();
        tap(&mut gesture, start);
        chord(&mut gesture, start);
        assert_eq!(
            key(&mut gesture, Key::Other(1), Edge::Down, start),
            [Some(Effect::Resting), None, None],
            "other key cancels listening"
        );
        assert_eq!(gesture.deadline(), None, "interruption cancels both timers");
        key(&mut gesture, Key::Other(1), Edge::Up, start);
        assert_eq!(
            key(&mut gesture, Key::LeftAlt, Edge::Up, start),
            [None; 3],
            "one modifier remains down"
        );
        assert_eq!(
            key(&mut gesture, Key::LeftAlt, Edge::Down, start),
            [None; 3],
            "repress cannot activate"
        );
        key(&mut gesture, Key::LeftAlt, Edge::Up, start);
        key(&mut gesture, Key::LeftControl, Edge::Up, start);
        tap(&mut gesture, start);
        assert_eq!(
            gesture.handle(Event::Deadline, start + WINDOW),
            [None; 3],
            "first tap expires"
        );
        assert_eq!(gesture.deadline(), None, "no timer after expiry");
    }

    #[test]
    fn altgr_does_not_activate_but_one_held_modifier_allows_double_tap() {
        let now = Instant::now();
        let mut gesture = Gesture::legacy_hold_fixture();
        key(&mut gesture, Key::LeftControl, Edge::Down, now);
        assert_eq!(
            key(&mut gesture, Key::Other(1), Edge::Down, now),
            [None; 3],
            "AltGr synthetic Ctrl plus right Alt"
        );
        assert!(!gesture.listening(), "AltGr is not a chord");
        assert_eq!(
            key(&mut gesture, Key::LeftAlt, Edge::Down, now),
            [None; 3],
            "cancelled chord stays blocked"
        );
        for held in [Key::LeftControl, Key::LeftAlt] {
            gesture.handle(Event::Reset, now);
            chord(&mut gesture, now);
            let tapped = if held == Key::LeftControl {
                Key::LeftAlt
            } else {
                Key::LeftControl
            };
            key(&mut gesture, tapped, Edge::Up, now);
            let second = now + Duration::from_millis(100);
            assert_eq!(
                key(&mut gesture, tapped, Edge::Down, second),
                [Some(Effect::Listening), None, None],
                "fresh make with one held modifier"
            );
            assert_eq!(
                key(&mut gesture, tapped, Edge::Up, second),
                [Some(Effect::Resting), Some(Effect::OpenInput), None],
                "completed second tap opens input"
            );
        }
    }

    #[test]
    fn held_other_keys_block_chords_until_every_key_is_released() {
        let now = Instant::now();
        let mut gesture = Gesture::legacy_hold_fixture();
        let right_alt = Key::Other(0x138);
        for other in [right_alt, right_alt, Key::Other(1023)] {
            assert_eq!(
                key(&mut gesture, other, Edge::Down, now),
                [None; 3],
                "other makes and repeats do not activate"
            );
        }
        for released in [right_alt, Key::Other(1023)] {
            for modifier in [Key::LeftControl, Key::LeftAlt] {
                assert_eq!(
                    key(&mut gesture, modifier, Edge::Down, now),
                    [None; 3],
                    "an already held other key blocks chord activation"
                );
            }
            assert!(
                !gesture.listening(),
                "no listening while another key is held"
            );
            assert_eq!(gesture.deadline(), None, "blocked chords do not arm timers");
            for modifier in [Key::LeftControl, Key::LeftAlt] {
                key(&mut gesture, modifier, Edge::Up, now);
            }
            assert_eq!(
                key(&mut gesture, released, Edge::Up, now),
                [None; 3],
                "one release balances repeated makes, without clearing another key"
            );
        }
        chord(&mut gesture, now);
    }

    #[test]
    fn invalid_other_identity_resets_and_cancels_without_indexing() {
        let now = Instant::now();
        let mut gesture = Gesture::legacy_hold_fixture();
        chord(&mut gesture, now);
        gesture.handle(Event::Deadline, now + WINDOW);
        assert_eq!(
            key(&mut gesture, Key::Other(1024), Edge::Down, now + WINDOW),
            [Some(Effect::HoldEnded), Some(Effect::Resting), None],
            "invalid identity cancels active hold"
        );
        assert_eq!(gesture.deadline(), None, "invalid identity clears timers");
        assert_eq!(
            key(&mut gesture, Key::Other(u16::MAX), Edge::Up, now + WINDOW),
            [None; 3],
            "invalid release also resets safely"
        );
        chord(&mut gesture, now + WINDOW);
    }

    #[test]
    fn reset_or_other_key_ends_hold_and_cancels_timer() {
        let start = Instant::now();
        for event in [
            Event::Reset,
            Event::Key {
                key: Key::Other(1),
                edge: Edge::Down,
            },
        ] {
            let mut gesture = Gesture::legacy_hold_fixture();
            chord(&mut gesture, start);
            gesture.handle(Event::Deadline, start + WINDOW);
            assert_eq!(
                gesture.handle(event, start + WINDOW),
                [Some(Effect::HoldEnded), Some(Effect::Resting), None],
                "active hold cancels"
            );
            assert_eq!(gesture.deadline(), None, "cancellation removes timer");
            assert_eq!(
                gesture.handle(Event::Deadline, start + WINDOW),
                [None; 3],
                "stale timer does nothing"
            );
            if event == Event::Reset {
                chord(&mut gesture, start + WINDOW);
            }
        }
    }
}
