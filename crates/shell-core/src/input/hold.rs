use std::cell::Cell;
use std::time::Instant;

use kumochi_protocol::message::GestureId;

use super::{Edge, Effect, Event, Gesture, Key};
use crate::platform::microphone::{CollectionGate, Generation, MicError};

#[derive(Debug)]
pub enum InputStage {
    Native,
    Gesture,
    Delivery,
}

#[derive(Debug)]
pub enum InputReason {
    Armed,
    AlreadyArmed,
    RawInputRegistration,
    SessionWatcher,
    DesktopWatcher,
    DeadlineArm,
    DeadlineArmed,
    DeadlineDelivered,
    DeadlineOverdue,
    RawInputRead,
    CallbackMissing,
    ClockUnavailable,
    ChordStarted,
    ChordSuppressed,
    HoldRecognized,
    Paused,
    Interrupted,
    Reset,
    Deferred,
    OwnerGone,
    QueueClosed,
}

/// Which event reached the hold threshold.
#[derive(Debug)]
enum Recognition {
    Deadline,
    Scheduled,
    Repeat,
    Release,
}

#[derive(Clone, Debug)]
pub enum Intent {
    Warm(GestureId),
    Recognized {
        gesture: GestureId,
        generation: Result<Generation, MicError>,
    },
    End {
        gesture: GestureId,
        cancelled: bool,
    },
    Unavailable,
}

#[derive(Clone, Debug)]
pub struct Delivery {
    pub effects: [Option<Effect>; 3],
    pub intents: [Option<Intent>; 3],
}

pub struct Fence {
    gate: CollectionGate,
    last: u32,
    current: Option<GestureId>,
    /// The current gesture began while paused, so it stays dropped to its end.
    dropped: bool,
}

impl Fence {
    pub fn new(gate: CollectionGate) -> Self {
        Self {
            gate,
            last: 0,
            current: None,
            dropped: false,
        }
    }

    /// While `paused` (the onboarding window shows), a hold is dropped before recognition: no
    /// warm-up, no armed collection and no intent, so nothing reaches the owner.
    pub fn reduce(
        &mut self,
        state: &mut Gesture,
        event: Event,
        now: Instant,
        paused: &Cell<bool>,
    ) -> Delivery {
        self.reduce_with_keyboard_read(state, event, now, paused, || None)
    }

    pub fn reduce_with_keyboard_read(
        &mut self,
        state: &mut Gesture,
        event: Event,
        now: Instant,
        paused: &Cell<bool>,
        read: impl FnOnce() -> Option<Instant>,
    ) -> Delivery {
        let now = state.recover(event, read).unwrap_or(now);
        let paused = paused.get();
        let reduced = state.recognize(event, now, |reason| {
            tracing::debug!(stage = ?InputStage::Gesture, ?reason, "held speech input boundary");
        });
        let cancelled = matches!(
            event,
            Event::Reset
                | Event::Escape
                | Event::Key {
                    key: Key::Escape(_) | Key::Other(_),
                    ..
                }
        );
        let mut intents = std::array::from_fn(|_| None);
        for (at, effect) in reduced.iter().enumerate() {
            intents[at] = match effect {
                Some(Effect::Listening) => {
                    self.current = self.last.checked_add(1).map(|id| {
                        self.last = id;
                        GestureId(id)
                    });
                    if self.current.is_none() {
                        self.gate.cancel_current();
                    }
                    self.dropped = paused;
                    (!paused).then(|| self.current.map_or(Intent::Unavailable, Intent::Warm))
                }
                Some(Effect::HoldStarted) => {
                    let by = match event {
                        Event::Key {
                            edge: Edge::Down, ..
                        } => Recognition::Repeat,
                        Event::Key { edge: Edge::Up, .. } => Recognition::Release,
                        Event::Scheduled => Recognition::Scheduled,
                        _ => Recognition::Deadline,
                    };
                    if paused || self.dropped {
                        self.current = None;
                        tracing::debug!(stage = ?InputStage::Gesture, reason = ?InputReason::Paused, ?by, "held speech input boundary");
                        continue;
                    }
                    tracing::debug!(stage = ?InputStage::Gesture, reason = ?InputReason::HoldRecognized, ?by, "held speech input boundary");
                    Some(self.current.map_or(Intent::Unavailable, |gesture| {
                        let generation = self.gate.arm(gesture);
                        if let Err(reason) = generation {
                            tracing::warn!(stage = ?InputStage::Gesture, ?reason, "held speech input failed");
                        }
                        Intent::Recognized { gesture, generation }
                    }))
                }
                Some(Effect::HoldEnded) => self.current.map(|gesture| {
                    if cancelled {
                        self.gate.cancel(gesture);
                    } else {
                        self.gate.release(gesture);
                    }
                    Intent::End { gesture, cancelled }
                }),
                _ => None,
            };
        }
        if matches!(event, Event::Reset | Event::Escape) {
            self.gate.cancel_current();
            if !intents
                .iter()
                .any(|intent| matches!(intent, Some(Intent::End { .. })))
            {
                intents[0] = self.current.map(|gesture| Intent::End {
                    gesture,
                    cancelled: true,
                });
            }
        }
        Delivery {
            effects: reduced.map(|effect| {
                effect.filter(|effect| matches!(effect, Effect::OpenInput | Effect::Resting))
            }),
            intents,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use super::*;
    use crate::input::Edge;
    use crate::platform::microphone::Mode;

    fn key(fence: &mut Fence, state: &mut Gesture, key: Key, edge: Edge, at: Instant) -> Delivery {
        fence.reduce(state, Event::Key { key, edge }, at, &Cell::new(false))
    }

    fn begin() -> (Fence, Gesture, Instant) {
        let mut fence = Fence::new(CollectionGate::default());
        let mut state = Gesture::default();
        let at = Instant::now();
        key(&mut fence, &mut state, Key::LeftControl, Edge::Down, at);
        let pressed = key(&mut fence, &mut state, Key::LeftAlt, Edge::Down, at);
        assert!(matches!(
            pressed.intents,
            [Some(Intent::Warm(GestureId(1))), None, None]
        ));
        assert_eq!(pressed.effects, [None; 3]);
        (fence, state, at)
    }

    fn recognized(delivery: &Delivery) -> Generation {
        let Some(Intent::Recognized {
            gesture,
            generation,
        }) = &delivery.intents[0]
        else {
            panic!("a recognized hold")
        };
        assert_eq!(*gesture, GestureId(1));
        generation.clone().unwrap()
    }

    #[test]
    fn threshold_release_disarms_before_a_deferred_start_can_apply() {
        let (mut fence, mut state, at) = begin();
        let release = key(
            &mut fence,
            &mut state,
            Key::LeftAlt,
            Edge::Up,
            at + Duration::from_millis(300),
        );
        let generation = recognized(&release);
        assert!(matches!(
            release.intents[1],
            Some(Intent::End {
                gesture: GestureId(1),
                cancelled: false
            })
        ));
        assert_eq!(
            generation.mode(&AtomicBool::new(true), Instant::now()),
            Ok(Mode::Release)
        );
        generation.discard_unstarted().unwrap();
    }

    #[test]
    fn native_reset_and_other_key_are_cancel_not_normal_release() {
        for event in [
            Event::Reset,
            Event::Key {
                key: Key::Other(crate::input::PhysicalKey::new(7).unwrap()),
                edge: Edge::Down,
            },
        ] {
            let (mut fence, mut state, at) = begin();
            let delivery = fence.reduce(
                &mut state,
                Event::Deadline,
                at + Duration::from_millis(300),
                &Cell::new(false),
            );
            let generation = recognized(&delivery);
            let terminal = fence.reduce(
                &mut state,
                event,
                at + Duration::from_millis(301),
                &Cell::new(false),
            );
            assert!(matches!(
                terminal.intents[0],
                Some(Intent::End {
                    gesture: GestureId(1),
                    cancelled: true
                })
            ));
            assert_eq!(
                generation.mode(&AtomicBool::new(true), Instant::now()),
                Err(MicError::Cancelled)
            );
            generation.discard_unstarted().unwrap();
        }
    }

    #[test]
    fn reset_after_release_still_cancels_unsubmitted_speech() {
        let (mut fence, mut state, at) = begin();
        let release = key(
            &mut fence,
            &mut state,
            Key::LeftAlt,
            Edge::Up,
            at + Duration::from_millis(300),
        );
        let generation = recognized(&release);
        let reset = fence.reduce(
            &mut state,
            Event::Reset,
            at + Duration::from_millis(301),
            &Cell::new(false),
        );
        assert!(matches!(
            reset.intents[0],
            Some(Intent::End {
                cancelled: true,
                ..
            })
        ));
        assert_eq!(
            generation.mode(&AtomicBool::new(true), Instant::now()),
            Err(MicError::Cancelled)
        );
    }

    #[test]
    fn exhausted_identifiers_cannot_wrap_or_arm() {
        let mut fence = Fence::new(CollectionGate::default());
        fence.last = u32::MAX;
        let mut state = Gesture::default();
        let at = Instant::now();
        key(&mut fence, &mut state, Key::LeftControl, Edge::Down, at);
        let pressed = key(&mut fence, &mut state, Key::LeftAlt, Edge::Down, at);
        assert!(matches!(pressed.intents[0], Some(Intent::Unavailable)));
        let hold = fence.reduce(
            &mut state,
            Event::Deadline,
            at + Duration::from_millis(300),
            &Cell::new(false),
        );
        assert!(matches!(hold.intents[0], Some(Intent::Unavailable)));
        assert_eq!(fence.last, u32::MAX);
    }

    /// The chord pressed at `at`, the threshold event at 300 ms and the chord released at 400 ms,
    /// each reduced with its own `paused` value.
    fn whole_hold(
        fence: &mut Fence,
        state: &mut Gesture,
        threshold: Event,
        at: Instant,
        paused: [bool; 3],
    ) -> Vec<Delivery> {
        let mut delivered = Vec::new();
        for key in [Key::LeftControl, Key::LeftAlt] {
            let edge = Edge::Down;
            delivered.push(fence.reduce(
                state,
                Event::Key { key, edge },
                at,
                &Cell::new(paused[0]),
            ));
        }
        let crossed = at + Duration::from_millis(300);
        delivered.push(fence.reduce(state, threshold, crossed, &Cell::new(paused[1])));
        for key in [Key::LeftAlt, Key::LeftControl] {
            let (edge, released) = (Edge::Up, at + Duration::from_millis(400));
            delivered.push(fence.reduce(
                state,
                Event::Key { key, edge },
                released,
                &Cell::new(paused[2]),
            ));
        }
        delivered
    }

    fn intents(delivered: &[Delivery]) -> Vec<Intent> {
        delivered
            .iter()
            .flat_map(|delivery| delivery.intents.iter().flatten().cloned())
            .collect()
    }

    #[test]
    fn a_paused_hold_is_dropped_on_every_recognition_path_and_the_next_one_is_not() {
        let gate = CollectionGate::default();
        let mut fence = Fence::new(gate.clone());
        let mut state = Gesture::default();
        let mut at = Instant::now();
        for threshold in [
            Event::Deadline,
            Event::Scheduled,
            Event::Key {
                key: Key::LeftAlt,
                edge: Edge::Down,
            },
            Event::Key {
                key: Key::LeftAlt,
                edge: Edge::Up,
            },
        ] {
            let armed = gate.latest();
            let paused = whole_hold(&mut fence, &mut state, threshold, at, [true; 3]);
            assert!(intents(&paused).is_empty(), "{threshold:?}");
            assert_eq!(gate.latest(), armed, "{threshold:?}");
            at += Duration::from_secs(2);
            let next = intents(&whole_hold(
                &mut fence, &mut state, threshold, at, [false; 3],
            ));
            let gesture = GestureId(fence.last);
            assert!(matches!(next[0], Intent::Warm(warm) if warm == gesture));
            let Intent::Recognized {
                gesture: recognised,
                generation: Ok(generation),
            } = &next[1]
            else {
                panic!("{threshold:?} recognises the next hold");
            };
            assert_eq!(*recognised, gesture);
            assert_eq!(gate.latest(), u64::from(gesture.0));
            generation.discard_unstarted().unwrap();
            at += Duration::from_secs(2);
        }
    }

    #[test]
    fn a_hold_that_meets_a_pause_on_either_side_of_its_press_stays_dropped() {
        let gate = CollectionGate::default();
        let mut fence = Fence::new(gate.clone());
        let mut state = Gesture::default();
        let at = Instant::now();
        let begun = whole_hold(
            &mut fence,
            &mut state,
            Event::Deadline,
            at,
            [true, false, false],
        );
        assert!(intents(&begun).is_empty());
        let later = at + Duration::from_secs(2);
        let paused = whole_hold(
            &mut fence,
            &mut state,
            Event::Deadline,
            later,
            [false, true, true],
        );
        assert!(matches!(intents(&paused)[..], [Intent::Warm(GestureId(2))]));
        assert_eq!(gate.latest(), 0);
    }

    #[test]
    fn a_hold_reaches_the_owner_only_as_resting_and_a_double_tap_still_opens_typing() {
        let mut fence = Fence::new(CollectionGate::default());
        let mut state = Gesture::default();
        let at = Instant::now();
        let hold = whole_hold(&mut fence, &mut state, Event::Deadline, at, [false; 3]);
        let effects: Vec<_> = hold.iter().map(|delivery| delivery.effects).collect();
        assert_eq!(
            effects,
            [
                [None; 3],
                [None; 3],
                [None; 3],
                [None, None, Some(Effect::Resting)],
                [None; 3],
            ]
        );
        let tap = at + Duration::from_secs(2);
        let mut steps = Vec::new();
        for (key, edge, after) in [
            (Key::LeftControl, Edge::Down, 0),
            (Key::LeftAlt, Edge::Down, 0),
            (Key::LeftAlt, Edge::Up, 0),
            (Key::LeftAlt, Edge::Down, 1),
            (Key::LeftAlt, Edge::Up, 2),
        ] {
            let at = tap + Duration::from_millis(after);
            steps.push(
                fence
                    .reduce(&mut state, Event::Key { key, edge }, at, &Cell::new(false))
                    .effects,
            );
        }
        assert_eq!(
            steps,
            [
                [None; 3],
                [None; 3],
                [Some(Effect::Resting), None, None],
                [None; 3],
                [Some(Effect::Resting), Some(Effect::OpenInput), None],
            ]
        );
    }
}
