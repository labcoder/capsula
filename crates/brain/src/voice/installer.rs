//! The voice installer's state: one entry per part, driven by the completions of the loads it
//! asks for. It does no I/O itself: each method returns the load the runtime starts, and every
//! report comes back with the ask it was started with. A report whose ask isn't the entry's
//! current one changes nothing.

#[cfg(test)]
mod tests;

use kumochi_protocol::message::{ErrorKey, PartState, VoiceView};
use kumochi_protocol::voice::VoicePart;

use super::{LoadEvent, LoadReport};

/// What `config.toml` says about a part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Durable {
    Absent,
    Installed,
    /// A removal was accepted and its folder couldn't be deleted yet.
    Removing,
}

/// A load for the runtime to start; the ask is the token its reports carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Load {
    pub part: VoicePart,
    pub ask: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Loading {
        ask: u64,
    },
    Ready {
        ask: u64,
    },
    /// Removing is durable with nothing in flight: its delete failed.
    Cleanup,
    Failed(ErrorKey),
}

#[derive(Debug)]
struct Entry {
    durable: Durable,
    phase: Phase,
    /// Automatic reloads left after a loss: one per session.
    retry_left: u8,
}

#[derive(Debug)]
pub(crate) struct Installer {
    parts: [Entry; 2],
    next_ask: u64,
}

impl Installer {
    /// The state at brain start, with no network. `whole` says every file of a part has its
    /// size, and `available` that its handle started; without one the part is a fixed load failure
    /// for the session. shortcut: a corrupt file of the right size still loads until the next
    /// install re-hashes it; hash at start once load failures point there.
    pub(crate) fn start(
        durable: [Durable; 2],
        whole: [bool; 2],
        available: [bool; 2],
    ) -> (Self, Vec<Load>) {
        let mut installer = Self {
            parts: durable.map(|durable| Entry {
                durable,
                phase: Phase::Idle,
                retry_left: 1,
            }),
            next_ask: 1,
        };
        let mut loads = Vec::new();
        for (index, part) in VoicePart::ALL.into_iter().enumerate() {
            let ask = installer.ask();
            let entry = &mut installer.parts[index];
            entry.phase = match (entry.durable, available[index], whole[index]) {
                (Durable::Removing, _, _) => Phase::Cleanup,
                (Durable::Absent, true, _) => Phase::Idle,
                (_, false, _) => Phase::Failed(ErrorKey::VoiceLoad),
                (Durable::Installed, true, false) => Phase::Failed(ErrorKey::VoiceCorrupt),
                (Durable::Installed, true, true) => {
                    loads.push(Load { part, ask });
                    Phase::Loading { ask }
                }
            };
        }
        (installer, loads)
    }

    pub(crate) fn view(&self) -> VoiceView {
        let [speech_input, spoken_replies] = &self.parts;
        VoiceView {
            speech_input: speech_input.state(),
            spoken_replies: spoken_replies.state(),
        }
    }

    /// Whether `part` is loaded and installed.
    pub(crate) fn ready(&self, part: VoicePart) -> bool {
        matches!(self.parts[index(part)].phase, Phase::Ready { .. })
    }

    /// A handle's report. A loss of the current model starts one automatic reload per session;
    /// a second loss, like a failed load, is a load failure.
    pub(crate) fn reported(&mut self, part: VoicePart, report: LoadReport) -> Option<Load> {
        let LoadReport { ask, event } = report;
        let entry = &mut self.parts[index(part)];
        match (entry.phase, event) {
            (Phase::Loading { ask: current }, LoadEvent::Loaded) if current == ask => {
                entry.phase = Phase::Ready { ask };
                None
            }
            (Phase::Loading { ask: current } | Phase::Ready { ask: current }, LoadEvent::Lost)
                if current == ask =>
            {
                self.lost(part)
            }
            (
                Phase::Loading { ask: current },
                LoadEvent::Failed | LoadEvent::Discarded | LoadEvent::Unloaded,
            ) if current == ask => {
                entry.phase = Phase::Failed(ErrorKey::VoiceLoad);
                None
            }
            _ => None,
        }
    }

    fn lost(&mut self, part: VoicePart) -> Option<Load> {
        let ask = self.ask();
        let entry = &mut self.parts[index(part)];
        if entry.retry_left == 0 {
            entry.phase = Phase::Failed(ErrorKey::VoiceLoad);
            return None;
        }
        entry.retry_left -= 1;
        entry.phase = Phase::Loading { ask };
        Some(Load { part, ask })
    }

    fn ask(&mut self) -> u64 {
        let ask = self.next_ask;
        self.next_ask += 1;
        ask
    }
}

fn index(part: VoicePart) -> usize {
    usize::from(part == VoicePart::SpokenReplies)
}

impl Entry {
    fn state(&self) -> PartState {
        match self.phase {
            Phase::Idle => PartState::NotInstalled,
            Phase::Loading { .. } => PartState::Loading,
            Phase::Ready { .. } => PartState::Ready,
            Phase::Cleanup => self.failed(ErrorKey::VoiceStorage),
            Phase::Failed(key) => self.failed(key),
        }
    }

    /// The one place `installed` is decided.
    fn failed(&self, key: ErrorKey) -> PartState {
        PartState::Failed {
            key,
            installed: self.durable == Durable::Installed,
        }
    }
}
