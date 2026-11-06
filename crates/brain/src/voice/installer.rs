//! The voice installer's state: one entry per part, driven by the user's install and cancel ops
//! and by the completions of the work it asks for. It does no I/O itself: every method returns
//! the [`Effect`]s the runtime carries out, and each completion comes back with the token it was
//! started with. A completion whose token isn't the entry's current one changes nothing.

#[cfg(test)]
mod tests;

use kumochi_protocol::message::{ErrorKey, PartState, VoiceView};
use kumochi_protocol::voice::VoicePart;

use super::download::{DownloadError, FetchPhase, Progress};
use super::{LoadEvent, LoadReport};

/// What `config.toml` says about a part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Durable {
    Absent,
    Installed,
    /// A removal was accepted and hasn't finished.
    Removing,
}

/// Work for the runtime. A load or unload uses the token as the handle's ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Starts the one fetch; its result comes back to `fetched`.
    Fetch {
        part: VoicePart,
        token: u64,
    },
    /// Stops the fetch in flight; its result still comes back.
    CancelFetch,
    Load {
        part: VoicePart,
        ask: u64,
    },
    Unload {
        part: VoicePart,
        ask: u64,
    },
    /// Queues the write that adds the part to `installed`; its result comes back to `activated`.
    Activate {
        part: VoicePart,
        token: u64,
    },
}

/// Where a cancelled install goes back to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rest {
    Idle,
    Failed(ErrorKey),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    /// Waiting for the fetch in flight to end.
    Queued {
        prior: Rest,
    },
    Fetching {
        prior: Rest,
        progress: Option<Progress>,
    },
    /// `prior` is `None` for start-up's load, which no install started.
    Loading {
        prior: Option<Rest>,
        ask: u64,
    },
    /// The Activate write is in flight.
    Committing {
        ask: u64,
    },
    Ready {
        ask: u64,
    },
    /// The `RemoveDone` write is in flight.
    Clearing,
    /// Removing is durable with nothing in flight: its delete or its `RemoveDone` failed.
    Cleanup,
    Failed(ErrorKey),
}

#[derive(Debug)]
struct Entry {
    durable: Durable,
    phase: Phase,
    token: u64,
    /// Automatic reloads left after a loss: one per install or session.
    retry_left: u8,
    /// The part's handle started; without it the part is a fixed load failure.
    available: bool,
    /// An install waiting for the `RemoveDone` write.
    install_after: bool,
}

#[derive(Debug)]
pub(crate) struct Installer {
    parts: [Entry; 2],
    /// The token of the fetch still running, cancelled or not. Only one runs at a time.
    fetching: Option<u64>,
    /// Quit began: no fetch starts again.
    quitting: bool,
    next_token: u64,
    effects: Vec<Effect>,
}

impl Installer {
    /// The state at brain start, with no network. `whole` says every file of a part has its
    /// size, and `available` that its handle started. `stuck` names the removals whose folders
    /// couldn't be deleted; any other `Removing` part has its `RemoveDone` write queued.
    /// shortcut: a corrupt file of the right size still loads until the next install re-hashes
    /// it; hash at start once load failures point there.
    pub(crate) fn start(
        durable: [Durable; 2],
        whole: [bool; 2],
        available: [bool; 2],
        stuck: &[VoicePart],
    ) -> (Self, Vec<Effect>) {
        let mut installer = Self {
            parts: [0, 1].map(|index| Entry {
                durable: durable[index],
                phase: Phase::Idle,
                token: 0,
                retry_left: 1,
                available: available[index],
                install_after: false,
            }),
            fetching: None,
            quitting: false,
            next_token: 1,
            effects: Vec::new(),
        };
        for (index, part) in VoicePart::ALL.into_iter().enumerate() {
            let token = installer.token();
            let entry = &mut installer.parts[index];
            entry.token = token;
            entry.phase = match (entry.durable, entry.available, whole[index]) {
                (Durable::Removing, _, _) if stuck.contains(&part) => Phase::Cleanup,
                (Durable::Removing, _, _) => Phase::Clearing,
                (Durable::Absent, true, _) => Phase::Idle,
                (_, false, _) => Phase::Failed(ErrorKey::VoiceLoad),
                (Durable::Installed, true, false) => Phase::Failed(ErrorKey::VoiceCorrupt),
                (Durable::Installed, true, true) => {
                    installer.effects.push(Effect::Load { part, ask: token });
                    Phase::Loading {
                        prior: None,
                        ask: token,
                    }
                }
            };
        }
        let effects = installer.take();
        (installer, effects)
    }

    pub(crate) fn view(&self) -> VoiceView {
        let [speech_input, spoken_replies] = &self.parts;
        VoiceView {
            speech_input: speech_input.state(),
            spoken_replies: spoken_replies.state(),
        }
    }

    /// Whether `part` is loaded with its install committed.
    pub(crate) fn ready(&self, part: VoicePart) -> bool {
        matches!(self.parts[index(part)].phase, Phase::Ready { .. })
    }

    /// `InstallVoice`, answered accepted with no write whatever it does. A failed part, installed
    /// or not, is fetched again, which re-hashes its files. Nothing fetches around an unfinished
    /// removal: an install waits for its `RemoveDone` write, and one after a failed removal does
    /// nothing until the next start finishes it.
    pub(crate) fn install(&mut self, part: VoicePart) -> Vec<Effect> {
        let entry = &mut self.parts[index(part)];
        let prior = match entry.phase {
            _ if !entry.available => None,
            Phase::Idle => Some(Rest::Idle),
            Phase::Failed(key) => Some(Rest::Failed(key)),
            Phase::Clearing => {
                entry.install_after = true;
                None
            }
            _ => None,
        };
        if let Some(prior) = prior {
            let token = self.token();
            let entry = &mut self.parts[index(part)];
            entry.token = token;
            entry.retry_left = 1;
            entry.phase = Phase::Queued { prior };
        }
        self.advance();
        self.take()
    }

    /// `CancelVoiceInstall`: every install still queued, fetching or loading goes back to where it
    /// was, with its files kept. A commit in flight isn't cancelled. Answered accepted with no
    /// write.
    pub(crate) fn cancel(&mut self) -> Vec<Effect> {
        for (index, part) in VoicePart::ALL.into_iter().enumerate() {
            let (prior, stop) = match self.parts[index].phase {
                Phase::Queued { prior } => (prior, None),
                Phase::Fetching { prior, .. } => (prior, Some(Effect::CancelFetch)),
                Phase::Loading {
                    prior: Some(prior),
                    ask,
                } => (prior, Some(Effect::Unload { part, ask })),
                _ => continue,
            };
            self.effects.extend(stop);
            let token = self.token();
            let entry = &mut self.parts[index];
            entry.token = token;
            entry.phase = match prior {
                Rest::Idle => Phase::Idle,
                Rest::Failed(key) => Phase::Failed(key),
            };
        }
        self.take()
    }

    /// Quit: the running fetch is told to stop, and no fetch starts after it, so an install still
    /// queued never downloads. A commit already queued still lands.
    pub(crate) fn quit(&mut self) -> Vec<Effect> {
        self.quitting = true;
        if self.fetching.is_some() {
            self.effects.push(Effect::CancelFetch);
        }
        self.take()
    }

    /// The latest progress of the fetch with `token`.
    pub(crate) fn progress(&mut self, part: VoicePart, token: u64, value: Progress) {
        let entry = &mut self.parts[index(part)];
        if entry.token == token
            && let Phase::Fetching { progress, .. } = &mut entry.phase
        {
            *progress = Some(value);
        }
    }

    /// A fetch ended. A current one loads or fails; any lets the next queued fetch start.
    pub(crate) fn fetched(
        &mut self,
        part: VoicePart,
        token: u64,
        result: Result<(), DownloadError>,
    ) -> Vec<Effect> {
        if self.fetching == Some(token) {
            self.fetching = None;
        }
        let entry = &mut self.parts[index(part)];
        if let Phase::Fetching { prior, .. } = entry.phase
            && entry.token == token
        {
            entry.phase = match result {
                Ok(()) => {
                    self.effects.push(Effect::Load { part, ask: token });
                    Phase::Loading {
                        prior: Some(prior),
                        ask: token,
                    }
                }
                Err(error) => Phase::Failed(fetch_key(error)),
            };
        }
        self.advance();
        self.take()
    }

    /// A handle's report. A loaded fresh install commits before it's ready; a retry of an
    /// installed part is ready at once. A loss of the current model starts one automatic reload;
    /// a second loss, like a failed load, is a load failure.
    pub(crate) fn reported(&mut self, part: VoicePart, report: LoadReport) -> Vec<Effect> {
        let LoadReport { ask, event } = report;
        let entry = &mut self.parts[index(part)];
        match (entry.phase, event) {
            (Phase::Loading { ask: current, .. }, LoadEvent::Loaded) if current == ask => {
                if entry.durable == Durable::Installed {
                    entry.phase = Phase::Ready { ask };
                } else {
                    entry.phase = Phase::Committing { ask };
                    self.effects.push(Effect::Activate {
                        part,
                        token: entry.token,
                    });
                }
            }
            (
                Phase::Loading { ask: current, .. }
                | Phase::Committing { ask: current }
                | Phase::Ready { ask: current },
                LoadEvent::Lost,
            ) if current == ask => self.lost(part),
            (
                Phase::Loading { ask: current, .. },
                LoadEvent::Failed | LoadEvent::Discarded | LoadEvent::Unloaded,
            ) if current == ask => entry.phase = Phase::Failed(ErrorKey::VoiceLoad),
            _ => {}
        }
        self.take()
    }

    /// The Activate write's result. What's on disk is mirrored whatever the token; the phase
    /// moves only for the current one. A refused write unloads the model it would have kept.
    pub(crate) fn activated(
        &mut self,
        part: VoicePart,
        token: u64,
        result: Result<(), ErrorKey>,
    ) -> Vec<Effect> {
        let entry = &mut self.parts[index(part)];
        if result.is_ok() {
            entry.durable = Durable::Installed;
        }
        let Phase::Committing { ask } = entry.phase else {
            return self.take();
        };
        if entry.token != token {
            return self.take();
        }
        match result {
            Ok(()) => entry.phase = Phase::Ready { ask },
            Err(key) => {
                let fresh = self.token();
                let entry = &mut self.parts[index(part)];
                entry.token = fresh;
                entry.phase = Phase::Failed(key);
                self.effects.push(Effect::Unload { part, ask });
            }
        }
        self.take()
    }

    /// The `RemoveDone` write's result. A failed one leaves the removal durable, in Cleanup.
    pub(crate) fn cleared(&mut self, part: VoicePart, result: Result<(), ErrorKey>) -> Vec<Effect> {
        let entry = &mut self.parts[index(part)];
        if result.is_ok() {
            entry.durable = Durable::Absent;
        }
        if entry.phase != Phase::Clearing {
            return self.take();
        }
        let install = std::mem::take(&mut entry.install_after);
        if result.is_err() {
            entry.phase = Phase::Cleanup;
            return self.take();
        }
        entry.phase = Phase::Idle;
        if install {
            return self.install(part);
        }
        self.take()
    }

    /// Starts the earliest queued install's fetch once no fetch is running, unless quitting.
    fn advance(&mut self) {
        if self.quitting || self.fetching.is_some() {
            return;
        }
        let next = VoicePart::ALL
            .into_iter()
            .zip(&mut self.parts)
            .filter(|(_, entry)| matches!(entry.phase, Phase::Queued { .. }))
            .min_by_key(|(_, entry)| entry.token);
        if let Some((part, entry)) = next
            && let Phase::Queued { prior } = entry.phase
        {
            entry.phase = Phase::Fetching {
                prior,
                progress: None,
            };
            self.fetching = Some(entry.token);
            self.effects.push(Effect::Fetch {
                part,
                token: entry.token,
            });
        }
    }

    /// A current loss: one automatic reload, then a load failure.
    fn lost(&mut self, part: VoicePart) {
        let token = self.token();
        let entry = &mut self.parts[index(part)];
        let prior = match entry.phase {
            Phase::Loading { prior, .. } => prior,
            _ => None,
        };
        entry.token = token;
        if entry.retry_left == 0 {
            entry.phase = Phase::Failed(ErrorKey::VoiceLoad);
            return;
        }
        entry.retry_left -= 1;
        entry.phase = Phase::Loading { prior, ask: token };
        self.effects.push(Effect::Load { part, ask: token });
    }

    fn token(&mut self) -> u64 {
        let token = self.next_token;
        self.next_token += 1;
        token
    }

    fn take(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }
}

fn index(part: VoicePart) -> usize {
    usize::from(part == VoicePart::SpokenReplies)
}

impl Entry {
    fn state(&self) -> PartState {
        match self.phase {
            Phase::Idle | Phase::Clearing => PartState::NotInstalled,
            Phase::Fetching {
                progress:
                    Some(Progress {
                        phase: FetchPhase::Downloading,
                        done,
                        total,
                    }),
                ..
            } => PartState::Downloading { done, total },
            Phase::Queued { .. } | Phase::Fetching { .. } => PartState::Verifying,
            Phase::Loading { .. } | Phase::Committing { .. } => PartState::Loading,
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

fn fetch_key(error: DownloadError) -> ErrorKey {
    match error {
        DownloadError::NoSpace { .. } => ErrorKey::VoiceNoSpace,
        DownloadError::Refused
        | DownloadError::Timeout
        | DownloadError::Transport
        | DownloadError::Cancelled => ErrorKey::VoiceDownload,
        DownloadError::Size | DownloadError::Hash => ErrorKey::VoiceCorrupt,
        DownloadError::Io => ErrorKey::VoiceStorage,
    }
}
