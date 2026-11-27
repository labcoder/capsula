//! Physical evidence for dormant custom chords, using the same gesture timing as the default.

use std::{fmt, time::Instant};

use kumochi_protocol::message::ShortcutChord;

use super::{ChordEdge, Edge, Effect, Phase, PhysicalKey, TalkHeld, Timing};

/// The sided modifiers in the normalized shortcut mask. Right Alt cannot participate.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    LeftControl,
    RightControl,
    LeftAlt,
    LeftShift,
    RightShift,
    LeftWin,
    RightWin,
}

impl Modifier {
    const fn bit(self) -> u8 {
        1 << match self {
            Self::LeftControl => 0,
            Self::RightControl => 1,
            Self::LeftAlt => 2,
            Self::LeftShift => 4,
            Self::RightShift => 5,
            Self::LeftWin => 6,
            Self::RightWin => 7,
        }
    }
}

/// A key's meaning, separate from its device-specific identity.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Modifier(Modifier),
    Function(u8),
    Other,
}

impl fmt::Debug for Modifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Modifier(..)")
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Key(..)")
    }
}

/// An ordered observation from one desktop, layout and input generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The adapter has positively established neutral for a new generation.
    Neutral,
    /// An eligible raw edge, with the exact set of bound participants proven down after
    /// the edge. Identities must be unique per physical key and device within the generation.
    Key {
        id: PhysicalKey,
        key: Key,
        edge: Edge,
        held: TalkHeld<4>,
    },
    /// A scheduled deadline with fresh proof of the held participants, not a timer alone.
    Deadline { due: Instant, held: TalkHeld<4> },
    /// Coverage or physical evidence couldn't be proved. Use this for Esc, right Alt/AltGr
    /// ambiguity, stale known makes, device or layout changes, disconnect and rebinding.
    Uncertain,
}

/// Pure gesture effects, with cancellation distinguished from a participating release.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reduction {
    pub effects: [Option<Effect>; 3],
    /// Revoke existing release and queued effects for this generation, even if no hold is active.
    pub cancelled: bool,
    /// Only a proven repeat of this exact, still-held key can be exempt from screen invalidation.
    pub repeat: Option<PhysicalKey>,
}

/// The custom input boundary. Creating it never registers keys or enables voice or screen input.
pub struct Shortcut {
    chord: ShortcutChord,
    timing: Timing,
    held: TalkHeld<4>,
    coverage: (u64, Option<Instant>),
}

impl fmt::Debug for Shortcut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Shortcut(..)")
    }
}

impl Shortcut {
    /// Starts disarmed. Use the current generation as the lower bound so callbacks
    /// queued before rebinding cannot arm a replacement.
    #[must_use]
    pub fn new(chord: ShortcutChord, generation_floor: u64) -> Self {
        Self {
            chord,
            timing: Timing {
                phase: Phase::Blocked,
                ..Timing::default()
            },
            held: TalkHeld([None; 4]),
            coverage: (generation_floor, None),
        }
    }

    /// The next shared hold or double-tap deadline.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.timing.deadline()
    }

    /// The exact participants remaining after this observation, including the function key.
    #[must_use]
    pub fn held(&self) -> TalkHeld<4> {
        self.held
    }

    /// The adapter must positively verify the desktop, layout, right-Alt-up state and
    /// participant evidence before issuing a key or deadline. A pending raw event, an
    /// unchanged layout or elapsed time alone is not evidence. Only a newer generation's
    /// neutral observation can rearm after loss; callbacks from retired generations are ignored.
    /// The adapter owns bounded active coverage: on expiry it must send `Uncertain`
    /// before accepting another edge. A gesture deadline is not a coverage watchdog.
    /// Use the observation's timestamp, not the time a deferred effect reaches its owner.
    pub fn reduce(&mut self, generation: u64, event: Event, now: Instant) -> Reduction {
        let (current, last) = self.coverage;
        if generation < current {
            return Reduction::default();
        }
        if generation == current
            && let Some(last) = last
        {
            if event == Event::Neutral || matches!(self.timing.phase, Phase::Blocked) {
                return Reduction::default();
            }
            if matches!(event, Event::Deadline { due, .. } if self.deadline() != Some(due)) {
                return Reduction::default();
            }
            if now < last {
                return self.cancel();
            }
            self.coverage.1 = Some(now);
            return match event {
                Event::Key {
                    id,
                    key,
                    edge,
                    held,
                } => self.key(id, key, edge, held, now),
                Event::Deadline { held, .. } => {
                    if !same_keys(self.held, held) {
                        return self.cancel();
                    }
                    Reduction {
                        effects: [self.timing.advance(now), None, None],
                        ..Reduction::default()
                    }
                }
                Event::Uncertain => self.cancel(),
                Event::Neutral => Reduction::default(),
            };
        }
        let cancelled = self.cancel();
        self.coverage = (generation, Some(now));
        if event == Event::Neutral {
            self.timing.phase = Phase::Ready;
        }
        cancelled
    }

    fn cancel(&mut self) -> Reduction {
        if matches!(self.timing.phase, Phase::Blocked) {
            return Reduction::default();
        }
        self.held = TalkHeld([None; 4]);
        Reduction {
            effects: self.timing.block(),
            cancelled: true,
            repeat: None,
        }
    }

    fn key(
        &mut self,
        id: PhysicalKey,
        key: Key,
        edge: Edge,
        proof: TalkHeld<4>,
        now: Instant,
    ) -> Reduction {
        let mask = self.chord.modifiers();
        let count = |bits: u8| usize::try_from(bits.count_ones()).expect("at most eight mask bits");
        let modifiers = count(mask);
        let index = match key {
            Key::Modifier(side) if mask & side.bit() != 0 => count(mask & (side.bit() - 1)),
            Key::Function(n) if self.chord.function() == Some(n) => modifiers,
            _ => return self.cancel(),
        };
        let (change, repeat) = match (edge, self.held.0[index]) {
            (Edge::Down, Some(owner)) if owner == id => (ChordEdge::Repeat, Some(id)),
            (Edge::Down, None) if !self.held.0.contains(&Some(id)) => {
                if matches!(key, Key::Function(_))
                    && !self.held.0[..modifiers].iter().all(Option::is_some)
                {
                    return self.cancel();
                }
                self.held.0[index] = Some(id);
                let complete = (matches!(key, Key::Function(_)) || self.chord.function().is_none())
                    && self.held.0[..modifiers].iter().all(Option::is_some);
                (
                    ChordEdge::Press {
                        complete,
                        clear: true,
                    },
                    None,
                )
            }
            (Edge::Up, Some(owner)) if owner == id => {
                self.held.0[index] = None;
                let neutral = self.held.0.iter().all(Option::is_none);
                (ChordEdge::Release { neutral }, None)
            }
            _ => return self.cancel(),
        };
        if !same_keys(self.held, proof) {
            return self.cancel();
        }
        Reduction {
            effects: self.timing.participant(change, now, |_| {}),
            cancelled: false,
            repeat,
        }
    }
}

fn same_keys(expected: TalkHeld<4>, proof: TalkHeld<4>) -> bool {
    expected.0.iter().flatten().count() == proof.0.iter().flatten().count()
        && expected
            .0
            .into_iter()
            .flatten()
            .all(|id| proof.0.contains(&Some(id)))
}

#[cfg(test)]
#[path = "shortcut_tests.rs"]
mod tests;
