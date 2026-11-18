//! The voice installer's state: one entry per part, driven by the user's install, cancel and
//! remove ops and by the completions of the work it asks for. It does no I/O itself: every method
//! returns the [`Effect`]s the runtime carries out, and each completion comes back with the token
//! it was started with. A completion whose token isn't the entry's current one changes nothing,
//! except that a removal holds its prior phase's completions while its intent is written, and
//! waits for all of the part's work to end before it deletes, cancelled work included.

#[cfg(test)]
mod removal_tests;
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
    /// Deletes the part's folder; the result comes back to `deleted`.
    Delete {
        part: VoicePart,
        token: u64,
    },
    /// Queues the write that takes the part out of `removing`; its result comes back to
    /// `cleared`.
    Clear {
        part: VoicePart,
    },
}

/// The one answer a `RemoveVoice` gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoveAnswer {
    /// Nothing is installed or on disk to remove.
    Refused,
    /// Accepted with no write: the removal was already accepted, or its cleanup is retried.
    Accepted,
    /// The removal intent with this token is staged in the op's place; its result is the answer,
    /// and goes to `intended`.
    Intent { token: u64 },
}

/// Whether a part's folder is on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Folder {
    Missing,
    Present,
}

/// Where a cancelled install goes back to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rest {
    Idle,
    Unloaded,
    Failed(ErrorKey),
}

/// A completion for the prior token that came while a removal intent was being written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Held {
    Fetched(Result<(), DownloadError>),
    Report(LoadEvent),
    Activated(Result<(), ErrorKey>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActivatingModel {
    Live,
    Lost,
    LostWhileOff,
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
    /// An off transition or cancelled reload still owns its ask until the terminal report.
    Unloading {
        ask: u64,
    },
    /// The Activate write is in flight. A loss meanwhile waits for its result.
    Committing {
        ask: u64,
        model: ActivatingModel,
    },
    Ready {
        ask: u64,
    },
    Unloaded,
    /// The removal intent is being written; the entry's `intent` keeps what it goes back to.
    Intent,
    /// The removal is on disk. Waiting for the part's fetch to end and for every model it asked
    /// for to go, cancelled ones included.
    Draining,
    Deleting,
    /// The `RemoveDone` write is in flight.
    Clearing,
    /// Removing is durable with nothing in flight: its delete or its `RemoveDone` failed.
    Cleanup,
    Failed(ErrorKey),
}

/// The phase and token before a removal intent, and what completed for that token since, in
/// arrival order.
#[derive(Debug)]
struct Intent {
    phase: Phase,
    token: u64,
    held: Vec<Held>,
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
    /// An install waiting for a removal to finish.
    install_after: bool,
    /// Set only in `Phase::Intent`.
    intent: Option<Intent>,
    /// The asks of every load started with no final report yet, whatever the phase: a cancel or
    /// an unload doesn't free a model until its report says so.
    models: Vec<u64>,
    #[cfg(test)]
    last_write: Option<(&'static str, u64, Result<(), ErrorKey>)>,
    #[cfg(test)]
    last_delete: Option<(u64, Folder)>,
}

#[cfg(test)]
pub(crate) struct Diagnostic<'a>(&'a Entry);

#[cfg(test)]
impl std::fmt::Debug for Diagnostic<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let entry = self.0;
        f.debug_struct("Part")
            .field("phase", &entry.phase)
            .field("token", &entry.token)
            .field("owners", &entry.models.len())
            .field("durable", &entry.durable)
            .field(
                "held",
                &entry.intent.as_ref().map(|intent| intent.held.len()),
            )
            .field("last_write", &entry.last_write)
            .field("last_delete", &entry.last_delete)
            .finish()
    }
}

#[derive(Debug)]
pub(crate) struct Installer {
    parts: [Entry; 2],
    /// The part and token of the fetch still running, cancelled or not. Only one runs at a time.
    fetching: Option<(VoicePart, u64)>,
    /// Quit began: no fetch starts again.
    quitting: bool,
    input_enabled: bool,
    next_token: u64,
    effects: Vec<Effect>,
}

impl Installer {
    #[cfg(test)]
    pub(crate) fn diagnostics(&self) -> [Diagnostic<'_>; 2] {
        self.parts.each_ref().map(Diagnostic)
    }

    /// The state at brain start, with no network. `whole` says every file of a part has its
    /// size, and `available` that its handle started. `stuck` names the removals whose folders
    /// couldn't be deleted; any other `Removing` part has its `RemoveDone` write queued.
    /// shortcut: a corrupt file of the right size still loads until the next install re-hashes
    /// it; hash at start once load failures point there.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the committed input residency choice"
    )]
    pub(crate) fn start(
        durable: [Durable; 2],
        whole: [Result<bool, ErrorKey>; 2],
        available: [bool; 2],
        stuck: &[VoicePart],
        input_enabled: bool,
    ) -> (Self, Vec<Effect>) {
        let mut installer = Self {
            parts: [0, 1].map(|index| Entry {
                durable: durable[index],
                phase: Phase::Idle,
                token: 0,
                retry_left: 1,
                available: available[index],
                install_after: false,
                intent: None,
                models: Vec::new(),
                #[cfg(test)]
                last_write: None,
                #[cfg(test)]
                last_delete: None,
            }),
            fetching: None,
            quitting: false,
            input_enabled,
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
                (_, _, Err(key)) => Phase::Failed(key),
                (Durable::Absent, true, _) => Phase::Idle,
                (_, false, _) => Phase::Failed(ErrorKey::VoiceLoad),
                (Durable::Installed, true, Ok(false)) => Phase::Failed(ErrorKey::VoiceCorrupt),
                (Durable::Installed, true, Ok(true))
                    if part == VoicePart::SpeechInput && !input_enabled =>
                {
                    Phase::Unloaded
                }
                (Durable::Installed, true, Ok(true)) => {
                    entry.models.push(token);
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

    /// Whether the Activate write for `token` may still be made: a removal since takes it away.
    pub(crate) fn activating(&self, part: VoicePart, token: u64) -> bool {
        let entry = &self.parts[index(part)];
        entry.available && entry.token == token && matches!(entry.phase, Phase::Committing { .. })
    }

    /// Whether `part` is loaded with its install committed. A removal intent still being
    /// written changes nothing yet.
    pub(crate) fn ready(&self, part: VoicePart) -> bool {
        let entry = &self.parts[index(part)];
        entry.available && matches!(entry.shown(), Phase::Ready { .. })
    }

    pub(crate) fn load_input_refusal(&self) -> Option<ErrorKey> {
        if !self.input_enabled {
            return Some(ErrorKey::HoldToTalkOff);
        }
        let entry = &self.parts[index(VoicePart::SpeechInput)];
        (self.quitting
            || entry.phase != Phase::Unloaded
            || entry.durable != Durable::Installed
            || !entry.available)
            .then_some(ErrorKey::VoiceUnavailable)
    }

    /// A connected key-down or an explicit Load reloads only installed, idle-unloaded input.
    pub(crate) fn reload_input(&mut self) -> Vec<Effect> {
        if self.load_input_refusal().is_some() {
            return self.take();
        }
        self.load_installed(VoicePart::SpeechInput)
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the committed input residency choice"
    )]
    pub(crate) fn set_input_enabled(&mut self, enabled: bool) -> Vec<Effect> {
        self.input_enabled = enabled;
        self.release_off_input(VoicePart::SpeechInput);
        self.take()
    }

    fn release_off_input(&mut self, part: VoicePart) {
        if part != VoicePart::SpeechInput || self.input_enabled {
            return;
        }
        let entry = &mut self.parts[index(part)];
        if !entry.available {
            return;
        }
        let (Phase::Ready { ask }
        | Phase::Loading {
            prior: None | Some(Rest::Unloaded),
            ask,
        }) = entry.phase
        else {
            return;
        };
        if entry.models.contains(&ask) {
            entry.phase = Phase::Unloading { ask };
            self.effects.push(Effect::Unload { part, ask });
        } else {
            entry.phase = Phase::Unloaded;
        }
    }

    fn load_installed(&mut self, part: VoicePart) -> Vec<Effect> {
        let ask = self.token();
        let entry = &mut self.parts[index(part)];
        entry.token = ask;
        entry.models.push(ask);
        entry.phase = Phase::Loading {
            prior: Some(Rest::Unloaded),
            ask,
        };
        self.effects.push(Effect::Load { part, ask });
        self.take()
    }

    /// Installs or repairs a part, retaining the user's explicit verify/fetch consent. Nothing
    /// fetches around an unfinished removal; a failed removal first retries its cleanup.
    pub(crate) fn install(&mut self, part: VoicePart) -> Vec<Effect> {
        if !self.parts[index(part)].available {
            return self.take();
        }
        if self.parts[index(part)].phase == Phase::Unloaded {
            if self.quitting || part == VoicePart::SpeechInput {
                return self.take();
            }
            return self.load_installed(part);
        }
        let entry = &mut self.parts[index(part)];
        let prior = match entry.phase {
            Phase::Intent
            | Phase::Draining
            | Phase::Deleting
            | Phase::Clearing
            | Phase::Cleanup => {
                entry.install_after = true;
                None
            }
            _ if !entry.available => None,
            Phase::Idle => Some(Rest::Idle),
            Phase::Failed(key) => Some(Rest::Failed(key)),
            _ => None,
        };
        self.retry_cleanup(part);
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

    /// `CancelVoiceInstall`: drops installs waiting for removal and returns installs still queued,
    /// fetching or loading to where they were, with their files kept. A commit in flight isn't
    /// cancelled, and neither is a removal.
    /// Answered accepted with no write.
    pub(crate) fn cancel(&mut self) -> Vec<Effect> {
        for (index, part) in VoicePart::ALL.into_iter().enumerate() {
            let entry = &mut self.parts[index];
            entry.install_after = false;
            if !entry.available {
                continue;
            }
            let held_by_removal = entry.intent.is_some();
            let phase = entry
                .intent
                .as_mut()
                .map_or(&mut entry.phase, |intent| &mut intent.phase);
            if let Phase::Loading {
                prior: Some(Rest::Unloaded),
                ask,
            } = *phase
            {
                *phase = Phase::Unloading { ask };
                self.effects.push(Effect::Unload { part, ask });
                continue;
            }
            let (prior, stop) = match *phase {
                Phase::Queued { prior } => (prior, None),
                Phase::Fetching { prior, .. } => (
                    prior,
                    self.fetching
                        .is_some_and(|(fetching, _)| fetching == part)
                        .then_some(Effect::CancelFetch),
                ),
                Phase::Loading {
                    prior: Some(prior),
                    ask,
                } => (prior, Some(Effect::Unload { part, ask })),
                _ => continue,
            };
            *phase = prior.phase();
            self.effects.extend(stop);
            if !held_by_removal {
                let token = self.token();
                self.parts[index].token = token;
            }
        }
        self.take()
    }

    /// `RemoveVoice`, where `folder` says whether the part's folder exists. A queued install is
    /// dropped. Nothing else happens before the intent commits: the prior phase keeps its model,
    /// its fetch and its view.
    pub(crate) fn remove(
        &mut self,
        part: VoicePart,
        folder: Folder,
    ) -> (RemoveAnswer, Vec<Effect>) {
        let entry = &self.parts[index(part)];
        if !entry.available && !entry.models.is_empty() {
            return (RemoveAnswer::Refused, self.take());
        }
        let token = self.token();
        let entry = &mut self.parts[index(part)];
        if let Phase::Queued { prior } = entry.phase {
            entry.phase = prior.phase();
        }
        let answer = match entry.phase {
            Phase::Intent
            | Phase::Draining
            | Phase::Deleting
            | Phase::Clearing
            | Phase::Cleanup => RemoveAnswer::Accepted,
            Phase::Idle | Phase::Failed(_)
                if entry.durable == Durable::Absent && folder == Folder::Missing =>
            {
                RemoveAnswer::Refused
            }
            phase => {
                entry.intent = Some(Intent {
                    phase,
                    token: entry.token,
                    held: Vec::new(),
                });
                entry.phase = Phase::Intent;
                entry.token = token;
                RemoveAnswer::Intent { token }
            }
        };
        self.retry_cleanup(part);
        (answer, self.take())
    }

    /// Quit: the running fetch is told to stop, and no fetch starts after it, so an install still
    /// queued never downloads. A commit or removal intent already queued still lands.
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
        if self.fetching == Some((part, token)) {
            self.fetching = None;
        }
        let entry = &mut self.parts[index(part)];
        if !entry.available {
            self.advance();
            return self.take();
        }
        if entry.hold(token, Held::Fetched(result)) {
            self.advance();
            return self.take();
        }
        match entry.phase {
            Phase::Fetching { prior, .. } if entry.token == token => {
                entry.phase = match result {
                    Ok(()) => {
                        entry.models.push(token);
                        self.effects.push(Effect::Load { part, ask: token });
                        Phase::Loading {
                            prior: Some(prior),
                            ask: token,
                        }
                    }
                    Err(error) => Phase::Failed(fetch_key(error)),
                };
            }
            Phase::Draining => self.drained(part),
            _ => {}
        }
        self.advance();
        self.take()
    }

    /// A handle's report. A loaded fresh install commits before it's ready; a retry of an
    /// installed part needs no new commit. A loss of the current model can start one automatic reload;
    /// a second loss, like a failed load, is a load failure. A draining part waits for the first
    /// report that its model is gone.
    pub(crate) fn reported(&mut self, part: VoicePart, report: LoadReport) -> Vec<Effect> {
        let LoadReport { ask, event } = report;
        let entry = &mut self.parts[index(part)];
        if event == LoadEvent::Unavailable {
            if entry.models.contains(&ask) {
                entry.available = false;
                entry.install_after = false;
                if self.fetching.is_some_and(|(fetched, _)| fetched == part) {
                    self.effects.push(Effect::CancelFetch);
                }
            }
            return self.take();
        }
        if !entry.available {
            return self.take();
        }
        if event != LoadEvent::Loaded {
            entry.models.retain(|&model| model != ask);
        }
        if entry.hold(ask, Held::Report(event)) {
            return self.take();
        }
        match (entry.phase, event) {
            (Phase::Draining, _) => self.drained(part),
            (
                Phase::Unloading { ask: current },
                LoadEvent::Unloaded
                | LoadEvent::Discarded
                | LoadEvent::Failed
                | LoadEvent::Lost
                | LoadEvent::ModelPath,
            ) if current == ask => entry.phase = Phase::Unloaded,
            (Phase::Ready { ask: current }, LoadEvent::Unloaded)
                if part == VoicePart::SpeechInput && current == ask =>
            {
                entry.phase = Phase::Unloaded;
            }
            (Phase::Loading { ask: current, .. }, LoadEvent::Loaded) if current == ask => {
                if entry.durable == Durable::Installed {
                    entry.phase = Phase::Ready { ask };
                } else {
                    entry.phase = Phase::Committing {
                        ask,
                        model: ActivatingModel::Live,
                    };
                    self.effects.push(Effect::Activate {
                        part,
                        token: entry.token,
                    });
                }
            }
            (
                Phase::Committing {
                    ask: current,
                    model: ActivatingModel::Live,
                },
                LoadEvent::Lost,
            ) if current == ask => {
                entry.phase = Phase::Committing {
                    ask,
                    model: if part == VoicePart::SpeechInput && !self.input_enabled {
                        ActivatingModel::LostWhileOff
                    } else {
                        ActivatingModel::Lost
                    },
                };
            }
            (
                Phase::Loading { ask: current, .. } | Phase::Ready { ask: current },
                LoadEvent::Lost,
            ) if current == ask => self.lost(part),
            (Phase::Loading { ask: current, .. }, LoadEvent::ModelPath) if current == ask => {
                entry.phase = Phase::Failed(ErrorKey::VoiceModelPath);
            }
            (
                Phase::Loading { ask: current, .. },
                LoadEvent::Failed | LoadEvent::Discarded | LoadEvent::Unloaded,
            ) if current == ask => entry.phase = Phase::Failed(ErrorKey::VoiceLoad),
            _ => {}
        }
        self.release_off_input(part);
        self.take()
    }

    /// The Activate write's result. What's on disk is mirrored whatever the token; the phase
    /// moves only for the current one. A refused write unloads the model it would have kept; a
    /// committed one can reload a model only if the loss was eligible at the time.
    pub(crate) fn activated(
        &mut self,
        part: VoicePart,
        token: u64,
        result: Result<(), ErrorKey>,
    ) -> Vec<Effect> {
        let entry = &mut self.parts[index(part)];
        #[cfg(test)]
        {
            entry.last_write = Some(("activate", token, result));
        }
        if result.is_ok() {
            entry.durable = Durable::Installed;
        }
        if !entry.available {
            return self.take();
        }
        if entry.hold(token, Held::Activated(result)) {
            return self.take();
        }
        let Phase::Committing { ask, model } = entry.phase else {
            return self.take();
        };
        if entry.token != token {
            return self.take();
        }
        match (result, model) {
            (Ok(()), ActivatingModel::Live) => entry.phase = Phase::Ready { ask },
            (Ok(()), ActivatingModel::Lost) => self.lost(part),
            (Ok(()), ActivatingModel::LostWhileOff) => entry.phase = Phase::Unloaded,
            (Err(key), _) => {
                let fresh = self.token();
                let entry = &mut self.parts[index(part)];
                entry.token = fresh;
                entry.phase = Phase::Failed(key);
                self.effects.push(Effect::Unload { part, ask });
            }
        }
        self.release_off_input(part);
        self.take()
    }

    /// The removal intent's result, which is also its `RemoveVoice`'s answer. Committed, the
    /// part's fetch is cancelled and every model it asked for is unloaded, cancelled ones
    /// included, and the folder is deleted once all of them have ended. Refused, the part goes
    /// back to its prior phase and token, and what completed for them meanwhile is applied in
    /// order, as if it had just come.
    pub(crate) fn intended(
        &mut self,
        part: VoicePart,
        token: u64,
        result: Result<(), ErrorKey>,
    ) -> Vec<Effect> {
        let entry = &mut self.parts[index(part)];
        #[cfg(test)]
        {
            entry.last_write = Some(("intent", token, result));
        }
        if result.is_ok() {
            entry.durable = Durable::Removing;
        }
        if entry.token != token || entry.phase != Phase::Intent {
            return self.take();
        }
        let Some(Intent {
            phase: prior,
            token: prior_token,
            held,
        }) = entry.intent.take()
        else {
            return self.take();
        };
        if result.is_ok() {
            if self.fetching.is_some_and(|(fetched, _)| fetched == part) {
                self.effects.push(Effect::CancelFetch);
            }
            let entry = &mut self.parts[index(part)];
            self.effects
                .extend(entry.models.iter().map(|&ask| Effect::Unload { part, ask }));
            entry.phase = Phase::Draining;
            self.drained(part);
            return self.take();
        }
        entry.phase = prior;
        entry.token = prior_token;
        let install = std::mem::take(&mut entry.install_after);
        for completion in held {
            let effects = match completion {
                Held::Fetched(result) => self.fetched(part, prior_token, result),
                Held::Report(event) => self.reported(
                    part,
                    LoadReport {
                        ask: prior_token,
                        event,
                    },
                ),
                Held::Activated(result) => self.activated(part, prior_token, result),
            };
            self.effects.extend(effects);
        }
        if install {
            let effects = self.install(part);
            self.effects.extend(effects);
        }
        self.release_off_input(part);
        self.take()
    }

    /// A delete's result, the folder as it left it: gone, the `RemoveDone` write follows; still
    /// there, the removal stays durable, in Cleanup, and an install waiting for it is dropped.
    pub(crate) fn deleted(&mut self, part: VoicePart, token: u64, folder: Folder) -> Vec<Effect> {
        let entry = &mut self.parts[index(part)];
        #[cfg(test)]
        {
            entry.last_delete = Some((token, folder));
        }
        if entry.token != token || entry.phase != Phase::Deleting {
            return self.take();
        }
        if folder == Folder::Missing {
            entry.phase = Phase::Clearing;
            self.effects.push(Effect::Clear { part });
        } else {
            entry.phase = Phase::Cleanup;
            entry.install_after = false;
        }
        self.take()
    }

    /// The `RemoveDone` write's result. A failed one leaves the removal durable, in Cleanup.
    pub(crate) fn cleared(&mut self, part: VoicePart, result: Result<(), ErrorKey>) -> Vec<Effect> {
        let entry = &mut self.parts[index(part)];
        #[cfg(test)]
        {
            entry.last_write = Some(("clear", entry.token, result));
        }
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

    /// From Cleanup, the same delete and `RemoveDone` again, with a new token.
    fn retry_cleanup(&mut self, part: VoicePart) {
        if self.parts[index(part)].phase != Phase::Cleanup {
            return;
        }
        let token = self.token();
        let entry = &mut self.parts[index(part)];
        entry.token = token;
        entry.phase = Phase::Deleting;
        self.effects.push(Effect::Delete { part, token });
    }

    /// Deletes once a draining part's fetch has ended and each of its models is gone.
    fn drained(&mut self, part: VoicePart) {
        let fetching = self.fetching.is_some_and(|(fetched, _)| fetched == part);
        let entry = &mut self.parts[index(part)];
        if entry.phase == Phase::Draining && !fetching && entry.models.is_empty() {
            entry.phase = Phase::Deleting;
            self.effects.push(Effect::Delete {
                part,
                token: entry.token,
            });
        }
    }

    /// Starts the earliest queued install's fetch once no fetch is running, unless quitting.
    fn advance(&mut self) {
        if self.quitting || self.fetching.is_some() {
            return;
        }
        let next = VoicePart::ALL
            .into_iter()
            .zip(&mut self.parts)
            .filter(|(_, entry)| entry.available && matches!(entry.phase, Phase::Queued { .. }))
            .min_by_key(|(_, entry)| entry.token);
        if let Some((part, entry)) = next
            && let Phase::Queued { prior } = entry.phase
        {
            entry.phase = Phase::Fetching {
                prior,
                progress: None,
            };
            self.fetching = Some((part, entry.token));
            self.effects.push(Effect::Fetch {
                part,
                token: entry.token,
            });
        }
    }

    /// A current loss: one automatic reload, then a load failure. Nothing reloads once quitting.
    fn lost(&mut self, part: VoicePart) {
        let token = self.token();
        let entry = &mut self.parts[index(part)];
        let prior = match entry.phase {
            Phase::Loading { prior, .. } => prior,
            _ => None,
        };
        entry.token = token;
        if self.quitting || entry.retry_left == 0 {
            entry.phase = Phase::Failed(ErrorKey::VoiceLoad);
            return;
        }
        if part == VoicePart::SpeechInput && !self.input_enabled {
            entry.phase = if entry.durable == Durable::Installed {
                Phase::Unloaded
            } else {
                Phase::Failed(ErrorKey::VoiceLoad)
            };
            return;
        }
        entry.retry_left -= 1;
        entry.models.push(token);
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

/// A part's place in `VoicePart::ALL`.
pub(crate) fn index(part: VoicePart) -> usize {
    usize::from(part == VoicePart::SpokenReplies)
}

impl Rest {
    fn phase(self) -> Phase {
        match self {
            Self::Idle => Phase::Idle,
            Self::Unloaded => Phase::Unloaded,
            Self::Failed(key) => Phase::Failed(key),
        }
    }
}

impl Entry {
    /// Holds a completion for the token a removal intent replaced; says whether it did.
    fn hold(&mut self, token: u64, completion: Held) -> bool {
        match &mut self.intent {
            Some(intent) if intent.token == token => {
                intent.held.push(completion);
                true
            }
            _ => false,
        }
    }

    /// The phase as the user sees it: the one before a removal intent until it commits.
    fn shown(&self) -> Phase {
        self.intent
            .as_ref()
            .map_or(self.phase, |intent| intent.phase)
    }

    fn state(&self) -> PartState {
        if !self.available && !self.models.is_empty() {
            return PartState::Unavailable;
        }
        match self.shown() {
            Phase::Idle | Phase::Intent | Phase::Draining | Phase::Deleting | Phase::Clearing => {
                PartState::NotInstalled
            }
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
            Phase::Loading { .. } | Phase::Unloading { .. } | Phase::Committing { .. } => {
                PartState::Loading
            }
            Phase::Ready { .. } => PartState::Ready,
            Phase::Unloaded => PartState::Unloaded,
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
        DownloadError::ModelPath => ErrorKey::VoiceModelPath,
    }
}
