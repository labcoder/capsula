use super::{Edge, Effect, Phase, PhysicalKey, Timing};
use std::time::Instant;

#[derive(Clone, Copy)]
pub(super) enum Modifier {
    Control,
    Alt,
    Shift,
    Super,
}

impl Modifier {
    const fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NamedKey {
    Letter(u8),
    Function(u8),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Chord {
    mask: u8,
    trigger: NamedKey,
}

impl Chord {
    pub(super) fn new(modifiers: &[Modifier], trigger: NamedKey) -> Option<Self> {
        let mask = modifiers.iter().fold(0, |mask, key| mask | key.bit());
        let valid = match trigger {
            NamedKey::Function(n) => (1..=24).contains(&n),
            NamedKey::Letter(key) => key.is_ascii_uppercase() && mask & !Modifier::Shift.bit() != 0,
        };
        valid.then_some(Self { mask, trigger })
    }
}

impl std::fmt::Debug for Chord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Chord(..)")
    }
}

#[derive(Clone, Copy)]
pub(super) enum Role {
    Modifier(Modifier),
    Named(NamedKey),
    Other,
}

pub(super) struct Shortcut {
    chord: Chord,
    timing: Timing,
    down: [u64; 16],
    modifiers: [Option<PhysicalKey>; 4],
    trigger: Option<PhysicalKey>,
    pressed: u16,
    lost: bool,
}

impl Shortcut {
    pub(super) fn new(chord: Chord) -> Self {
        Self {
            chord,
            timing: Timing::default(),
            down: [0; 16],
            modifiers: [None; 4],
            trigger: None,
            pressed: 0,
            lost: false,
        }
    }

    pub(super) fn lose_input(&mut self) -> [Option<Effect>; 3] {
        self.lost = true;
        self.timing.cancel()
    }

    pub(super) fn trusted_neutral(&mut self) {
        *self = Self::new(self.chord);
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.timing.deadline()
    }

    pub(super) fn timer(&mut self, now: Instant) -> [Option<Effect>; 3] {
        [self.timing.advance(now), None, None]
    }

    pub(super) fn key(
        &mut self,
        id: PhysicalKey,
        role: Role,
        edge: Edge,
        now: Instant,
    ) -> [Option<Effect>; 3] {
        if self.lost {
            return [None; 3];
        }
        let word = usize::from(id.0 / 64);
        let bit = 1u64 << (id.0 % 64);
        let held = self.down[word] & bit != 0;
        let modifier = self.modifiers.iter().position(|slot| *slot == Some(id));
        let participating = modifier.is_some() || self.trigger == Some(id);
        if edge == Edge::Up {
            if !held {
                return [None; 3];
            }
            self.down[word] &= !bit;
            self.pressed -= 1;
            if let Some(index) = modifier {
                self.modifiers[index] = None;
            }
            if self.trigger == Some(id) {
                self.trigger = None;
            }
            if participating && matches!(self.timing.phase, Phase::Active { .. }) {
                let threshold = self.timing.advance(now);
                return self.timing.finish(now, threshold);
            }
            if self.pressed == 0 {
                self.timing.phase = Phase::Ready;
            }
            return [None; 3];
        }
        if held {
            return if participating {
                self.timer(now)
            } else {
                [None; 3]
            };
        }
        self.down[word] |= bit;
        self.pressed += 1;
        if let Role::Modifier(key) = role
            && self.chord.mask & key.bit() != 0
            && self.modifiers[key as usize].is_none()
        {
            self.modifiers[key as usize] = Some(id);
            return [None; 3];
        }
        if matches!(role, Role::Named(key) if key == self.chord.trigger)
            && matches!(self.timing.phase, Phase::Ready)
            && self.trigger.is_none()
            && u32::from(self.pressed) == self.chord.mask.count_ones() + 1
            && self
                .modifiers
                .iter()
                .enumerate()
                .all(|(i, slot)| slot.is_some() == (self.chord.mask & (1 << i) != 0))
        {
            self.trigger = Some(id);
            self.timing.expire_tap(now);
            return self.timing.start(now);
        }
        self.timing.cancel()
    }
}

pub(super) enum Trial {
    Capture,
    WaitForNeutral(Chord),
    Test(Shortcut),
    WaitToConfirm(Chord),
    Passed(Chord),
    Ended,
}

impl Trial {
    pub(super) fn capture(&mut self, chord: Chord) {
        if matches!(self, Self::Capture) {
            *self = Self::WaitForNeutral(chord);
        }
    }

    pub(super) fn trusted_neutral(&mut self) {
        match *self {
            Self::WaitForNeutral(chord) => *self = Self::Test(Shortcut::new(chord)),
            Self::WaitToConfirm(chord) => *self = Self::Passed(chord),
            _ => {}
        }
    }

    pub(super) fn preview(&mut self, id: PhysicalKey, role: Role, edge: Edge, now: Instant) {
        if let Self::Test(recognizer) = self {
            let effects = recognizer.key(id, role, edge, now);
            if effects.contains(&Some(Effect::Resting))
                && matches!(recognizer.timing.phase, Phase::Ready)
            {
                *self = Self::WaitToConfirm(recognizer.chord);
            }
        }
    }

    pub(super) fn confirm(&mut self) -> Option<Chord> {
        let Self::Passed(chord) = *self else {
            return None;
        };
        self.end();
        Some(chord)
    }

    pub(super) fn end(&mut self) {
        *self = Self::Ended;
    }
}

#[path = "shortcut_tests.rs"]
mod tests;
