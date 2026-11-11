use std::time::Instant;

use kumochi_protocol::message::GestureId;

use super::{Effect, Event, Gesture, Key};
use crate::platform::microphone::{CollectionGate, Generation, MicError};

#[derive(Clone, Debug)]
pub(crate) enum Intent {
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
pub(crate) struct Delivery {
    pub(crate) effects: [Option<Effect>; 3],
    pub(crate) intents: [Option<Intent>; 3],
}

pub(crate) struct Fence {
    gate: CollectionGate,
    last: u32,
    current: Option<GestureId>,
}

impl Fence {
    pub(crate) fn new(gate: CollectionGate) -> Self {
        Self {
            gate,
            last: 0,
            current: None,
        }
    }

    pub(crate) fn reduce(&mut self, state: &mut Gesture, event: Event, now: Instant) -> Delivery {
        let reduced = state.recognize(event, now);
        let cancelled = matches!(
            event,
            Event::Reset
                | Event::Key {
                    key: Key::Other(_),
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
                    Some(self.current.map_or(Intent::Unavailable, Intent::Warm))
                }
                Some(Effect::HoldStarted) => Some(self.current.map_or(
                    Intent::Unavailable,
                    |gesture| Intent::Recognized {
                        gesture,
                        generation: self.gate.arm(gesture),
                    },
                )),
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
        if matches!(event, Event::Reset) {
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
        fence.reduce(state, Event::Key { key, edge }, at)
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
                key: Key::Other(7),
                edge: Edge::Down,
            },
        ] {
            let (mut fence, mut state, at) = begin();
            let delivery =
                fence.reduce(&mut state, Event::Deadline, at + Duration::from_millis(300));
            let generation = recognized(&delivery);
            let terminal = fence.reduce(&mut state, event, at + Duration::from_millis(301));
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
        let reset = fence.reduce(&mut state, Event::Reset, at + Duration::from_millis(301));
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
        let hold = fence.reduce(&mut state, Event::Deadline, at + Duration::from_millis(300));
        assert!(matches!(hold.intents[0], Some(Intent::Unavailable)));
        assert_eq!(fence.last, u32::MAX);
    }
}
