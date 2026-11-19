//! Left Ctrl+Alt gestures, independent of native event delivery and the UI.
//!
//! A tap runs from chord make to its first modifier release. Two completed short taps
//! open input when their releases are less than 300ms apart. Either modifier may stay
//! held between taps. After another key cancels, all tracked keys must release to rearm.
//! A second tap that misses the window becomes a new first tap if it is still short.
//! Holding starts at 300ms inclusive, even when the release arrives before its timer.
//! Callers deliver monotonic timestamps and schedule only the next returned deadline.

use std::time::{Duration, Instant};

pub mod hold;
pub mod path;

#[cfg(test)]
mod shortcut;

const WINDOW: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PhysicalKey(u16);

impl PhysicalKey {
    pub const fn new(slot: u16) -> Option<Self> {
        if slot < 1024 { Some(Self(slot)) } else { None }
    }
}

impl std::fmt::Debug for PhysicalKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PhysicalKey(..)")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Key {
    LeftControl,
    LeftAlt,
    /// The adapter must give Escape a slot no other key uses.
    Escape(PhysicalKey),
    Other(PhysicalKey),
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(..)")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Down,
    Up,
}

impl std::fmt::Debug for Edge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Edge(..)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Key {
        key: Key,
        edge: Edge,
    },
    /// The native one-shot timer for the gesture's deadline.
    Deadline,
    /// GPUI's timer for the same deadline, which a busy native queue can't starve.
    Scheduled,
    Reset,
    Escape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Listening,
    Resting,
    HoldStarted,
    HoldEnded,
    OpenInput,
}

/// The physical modifiers that stayed down through release, redacted in diagnostics.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct TalkHeld(pub [Option<PhysicalKey>; 2]);

impl std::fmt::Debug for TalkHeld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TalkHeld").finish_non_exhaustive()
    }
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

#[derive(Default, Clone, Copy)]
pub struct Gesture {
    down: [bool; 2],
    other: [u64; 16],
    timing: Timing,
}

impl std::fmt::Debug for Gesture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Gesture(..)")
    }
}

#[derive(Default, Clone, Copy)]
struct Timing {
    phase: Phase,
    first_tap: Option<Instant>,
}

impl Gesture {
    #[cfg(any(test, feature = "test-support"))]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only this crate's tests read it")
    )]
    pub(crate) fn holding(&self) -> bool {
        matches!(self.timing.phase, Phase::Active { holding: true, .. })
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.timing.deadline()
    }

    /// The chord is down, or its first tap's window is still open.
    pub fn engaged(&self) -> bool {
        matches!(self.timing.phase, Phase::Active { .. }) || self.timing.first_tap.is_some()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn handle(&mut self, event: Event, now: Instant) -> [Option<Effect>; 3] {
        self.recognize(event, now, |_| {})
    }

    fn reset(&mut self) {
        *self = Self::default();
    }

    fn recognize(
        &mut self,
        event: Event,
        now: Instant,
        mut report: impl FnMut(hold::InputReason),
    ) -> [Option<Effect>; 3] {
        if matches!(event, Event::Reset | Event::Escape) {
            if !matches!(self.timing.phase, Phase::Ready) {
                report(hold::InputReason::Reset);
            }
            let effects = self.timing.cancel();
            self.reset();
            return effects;
        }
        let Event::Key { key, edge } = event else {
            return [self.timing.advance(now), None, None];
        };
        let index = match key {
            Key::LeftControl => 0,
            Key::LeftAlt => 1,
            Key::Escape(key) | Key::Other(key) => {
                if edge == Edge::Down && matches!(self.timing.phase, Phase::Active { .. }) {
                    report(hold::InputReason::Interrupted);
                }
                return self.other_key(key, edge);
            }
        };
        let down = edge == Edge::Down;
        // shortcut: a held key's auto-repeat also reaches the threshold if both deadline timers are
        // late, but only after the keyboard's repeat delay (about 500 ms, up to 1 s), so such a
        // hold starts late; drop it once desk logs never show `by=Repeat`.
        if self.down[index] == down {
            return [self.timing.advance(now), None, None];
        }
        let threshold = self.timing.advance(now);
        self.down[index] = down;
        if down && self.down == [true; 2] {
            let reason = if matches!(self.timing.phase, Phase::Ready) && self.other == [0; 16] {
                hold::InputReason::ChordStarted
            } else {
                hold::InputReason::ChordSuppressed
            };
            report(reason);
        }
        if down
            && matches!(self.timing.phase, Phase::Ready)
            && self.down == [true; 2]
            && self.other == [0; 16]
        {
            return self.timing.start(now);
        }
        if !down {
            if matches!(self.timing.phase, Phase::Active { .. }) {
                return self.timing.finish(now, threshold);
            }
            if self.down == [false; 2] && self.other == [0; 16] {
                self.timing.phase = Phase::Ready;
            }
        }
        [threshold, None, None]
    }

    fn other_key(&mut self, key: PhysicalKey, edge: Edge) -> [Option<Effect>; 3] {
        let word = usize::from(key.0 / 64);
        let mask = 1u64 << (key.0 % 64);
        if edge == Edge::Down {
            if self.other[word] & mask != 0 {
                return [None; 3];
            }
            self.other[word] |= mask;
            return self.timing.cancel();
        }
        self.other[word] &= !mask;
        if self.down == [false; 2] && self.other == [0; 16] {
            self.timing.phase = Phase::Ready;
        }
        [None; 3]
    }
}

impl Timing {
    fn deadline(&self) -> Option<Instant> {
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

    fn start(&mut self, now: Instant) -> [Option<Effect>; 3] {
        self.phase = Phase::Active {
            started: now,
            holding: false,
        };
        [Some(Effect::Listening), None, None]
    }

    fn finish(&mut self, now: Instant, threshold: Option<Effect>) -> [Option<Effect>; 3] {
        let holding = matches!(self.phase, Phase::Active { holding: true, .. });
        self.phase = Phase::Ready;
        if holding {
            return [threshold, Some(Effect::HoldEnded), Some(Effect::Resting)];
        }
        let open = self.first_tap.take().map(|_| Effect::OpenInput);
        if open.is_none() {
            self.first_tap = Some(now);
        }
        [Some(Effect::Resting), open, None]
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

    fn expire_tap(&mut self, now: Instant) {
        if self
            .first_tap
            .is_some_and(|released| now >= released + WINDOW)
        {
            self.first_tap = None;
        }
    }

    fn advance(&mut self, now: Instant) -> Option<Effect> {
        self.expire_tap(now);
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
    use super::{Edge, Effect, Event, Gesture, Key, PhysicalKey, TalkHeld, WINDOW};
    use std::time::{Duration, Instant};

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
    fn the_other_modifier_stays_down_until_its_own_break() {
        let now = Instant::now();
        for (released, other, held) in [
            (Key::LeftControl, Key::LeftAlt, [false, true]),
            (Key::LeftAlt, Key::LeftControl, [true, false]),
        ] {
            let mut gesture = Gesture::default();
            assert_eq!(gesture.down, [false; 2]);
            chord(&mut gesture, now);
            assert_eq!(
                gesture.handle(Event::Deadline, now + WINDOW),
                [Some(Effect::HoldStarted), None, None]
            );
            assert_eq!(
                key(&mut gesture, released, Edge::Up, now + WINDOW),
                [None, Some(Effect::HoldEnded), Some(Effect::Resting)]
            );
            assert_eq!(gesture.down, held);
            // The held key repeats, then comes up.
            key(&mut gesture, other, Edge::Down, now + WINDOW);
            assert_eq!(gesture.down, held);
            key(&mut gesture, other, Edge::Up, now + WINDOW);
            assert_eq!(gesture.down, [false; 2]);
        }
        let mut gesture = Gesture::default();
        chord(&mut gesture, now);
        assert_eq!(gesture.down, [true; 2]);
        gesture.handle(Event::Reset, now);
        assert_eq!(gesture.down, [false; 2]);
    }

    #[test]
    fn held_physical_modifiers_are_redacted() {
        let both = TalkHeld([PhysicalKey::new(7), PhysicalKey::new(511)]);
        assert_eq!(format!("{both:?}"), "TalkHeld { .. }");
        assert_eq!(format!("{:?}", TalkHeld::default()), "TalkHeld { .. }");
    }

    #[test]
    fn new_panels_can_inherit_capture_exclusion_only_during_a_recognized_hold() {
        let now = Instant::now();
        let mut gesture = Gesture::default();
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
                let mut gesture = Gesture::default();
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
            }
        }
    }

    #[test]
    fn a_chord_key_repeat_starts_the_hold_once_and_only_from_the_threshold() {
        let start = Instant::now();
        let at = |millis| start + Duration::from_millis(millis);
        let mut gesture = Gesture::default();
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
        let mut ready = Gesture::default();
        assert_eq!(key(&mut ready, Key::LeftAlt, Edge::Down, start), [None; 3]);
        assert_eq!(
            key(&mut ready, Key::LeftAlt, Edge::Down, at(400)),
            [None; 3],
            "a repeat while ready"
        );
        let mut releasing = Gesture::default();
        chord(&mut releasing, start);
        assert_eq!(
            key(
                &mut releasing,
                Key::Other(PhysicalKey::new(0x1E).unwrap()),
                Edge::Down,
                at(10)
            ),
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
            let mut gesture = Gesture::default();
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
        let mut gesture = Gesture::default();
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
            let mut gesture = Gesture::default();
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
        let mut gesture = Gesture::default();
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
        let mut gesture = Gesture::default();
        tap(&mut gesture, start);
        chord(&mut gesture, start);
        assert_eq!(
            key(
                &mut gesture,
                Key::Other(PhysicalKey::new(1).unwrap()),
                Edge::Down,
                start
            ),
            [Some(Effect::Resting), None, None],
            "other key cancels listening"
        );
        assert_eq!(gesture.deadline(), None, "interruption cancels both timers");
        key(
            &mut gesture,
            Key::Other(PhysicalKey::new(1).unwrap()),
            Edge::Up,
            start,
        );
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
        let mut gesture = Gesture::default();
        key(&mut gesture, Key::LeftControl, Edge::Down, now);
        assert_eq!(
            key(
                &mut gesture,
                Key::Other(PhysicalKey::new(1).unwrap()),
                Edge::Down,
                now
            ),
            [None; 3],
            "AltGr synthetic Ctrl plus right Alt"
        );
        assert_eq!(gesture.deadline(), None, "AltGr is not a chord");
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
        let mut gesture = Gesture::default();
        let right_alt = Key::Other(PhysicalKey::new(0x138).unwrap());
        for other in [
            right_alt,
            right_alt,
            Key::Other(PhysicalKey::new(1023).unwrap()),
        ] {
            assert_eq!(
                key(&mut gesture, other, Edge::Down, now),
                [None; 3],
                "other makes and repeats do not activate"
            );
        }
        for released in [right_alt, Key::Other(PhysicalKey::new(1023).unwrap())] {
            for modifier in [Key::LeftControl, Key::LeftAlt] {
                assert_eq!(
                    key(&mut gesture, modifier, Edge::Down, now),
                    [None; 3],
                    "an already held other key blocks chord activation"
                );
            }
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
    fn reset_or_other_key_ends_hold_and_cancels_timer() {
        let start = Instant::now();
        for event in [
            Event::Reset,
            Event::Key {
                key: Key::Other(PhysicalKey::new(1).unwrap()),
                edge: Edge::Down,
            },
        ] {
            let mut gesture = Gesture::default();
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
