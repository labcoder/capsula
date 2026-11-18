//! One explicit paste through the brain's authority and the shell's credential writer.

use std::fmt;
use std::mem;
use std::time::{Duration, Instant};

use kumochi_protocol::message::{
    ConnectionOp, ControlToShell, KeyRejection, KeyStageFailure, KeyStageId, KeyView,
};
use zeroize::Zeroizing;

use crate::platform::credential::{PastedAnthropicKey, StageError, StagePermit, write_key_stage};

const MAX_PASTE_BYTES: usize = 2048;
const PASTE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub session: u64,
    pub request: u64,
    pub deadline: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteFailure {
    Clipboard,
    TooLong,
    Rejected(KeyRejection),
    Unsaved,
    Unconfirmed,
    Offline,
}

#[derive(Default)]
enum Stage {
    #[default]
    Idle,
    Asked(Attempt, PastedAnthropicKey),
    Writing(Attempt, KeyStageId),
    Saving(Attempt, KeyStageId),
}

impl Stage {
    fn attempt(&self) -> Option<Attempt> {
        match self {
            Self::Idle => None,
            Self::Asked(at, _) | Self::Writing(at, _) | Self::Saving(at, _) => Some(*at),
        }
    }
}

pub struct KeyWriteJob {
    at: Attempt,
    plan: KeyStageId,
    permit: StagePermit,
    key: PastedAnthropicKey,
}

pub struct KeyWriteResult {
    at: Attempt,
    plan: KeyStageId,
    result: Result<(), StageError>,
}

impl KeyWriteJob {
    pub fn run(self) -> KeyWriteResult {
        self.run_with(Instant::now(), write_key_stage)
    }

    /// Completes a staged write in shell tests without calling the credential store.
    #[cfg(feature = "test-support")]
    pub fn simulate_write(self, now: Instant) -> KeyWriteResult {
        let plan = self.plan;
        self.run_with(now, |_, _| Ok(plan))
    }

    fn run_with(
        self,
        now: Instant,
        write: impl FnOnce(StagePermit, PastedAnthropicKey) -> Result<KeyStageId, StageError>,
    ) -> KeyWriteResult {
        let Self {
            at,
            plan,
            permit,
            key,
        } = self;
        KeyWriteResult {
            at,
            plan,
            result: if now >= at.deadline {
                Err(StageError::Expired)
            } else {
                write(permit, key).and_then(|written| {
                    if written == plan {
                        Ok(())
                    } else {
                        Err(StageError::IdentityMismatch)
                    }
                })
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Removal {
    #[default]
    None,
    Waiting,
    Retry,
}

#[derive(Default)]
pub struct KeyPaste {
    stage: Stage,
    request: u64,
    revision: u64,
    view: Option<KeyView>,
    saved: bool,
    removing: bool,
    failure: Option<PasteFailure>,
}

impl fmt::Debug for KeyPaste {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyPaste")
            .field("busy", &self.saving())
            .field("view", &self.view)
            .field("removal", &self.removal())
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

impl KeyPaste {
    pub fn receive(&mut self, view: KeyView) -> bool {
        let changed = self.view != Some(view);
        self.view = Some(view);
        if matches!(view, KeyView::Missing) {
            self.saved = false;
            self.removing = false;
        } else if matches!(view, KeyView::Ready | KeyView::Rejected(_)) {
            self.saved = true;
        }
        if changed {
            self.revision = self.revision.wrapping_add(1);
        }
        changed
    }

    pub fn disconnected(&mut self) {
        *self = Self {
            request: self.request,
            revision: self.revision.wrapping_add(1),
            ..Self::default()
        };
    }

    pub fn view(&self) -> Option<KeyView> {
        self.view
    }
    pub fn saved(&self) -> bool {
        self.saved
    }
    pub fn failure(&self) -> Option<PasteFailure> {
        self.failure
    }
    pub fn removal(&self) -> Removal {
        if !self.removing {
            Removal::None
        } else if self.view == Some(KeyView::Unconfirmed) {
            Removal::Retry
        } else {
            Removal::Waiting
        }
    }
    pub fn saving(&self) -> bool {
        self.stage.attempt().is_some()
    }

    pub fn can_paste(&self) -> bool {
        self.available() && self.failure != Some(PasteFailure::Unconfirmed)
    }

    fn available(&self) -> bool {
        self.view.is_some_and(|view| view != KeyView::Checking) && !self.saving() && !self.removing
    }

    pub fn begin(
        &mut self,
        text: Option<Zeroizing<String>>,
        session: u64,
        now: Instant,
        send: impl FnOnce(ConnectionOp) -> bool,
    ) -> Option<Attempt> {
        if !self.can_paste() {
            return None;
        }
        self.revision = self.revision.wrapping_add(1);
        self.failure = None;
        let Some(text) = text.filter(|text| !text.is_empty()) else {
            self.failure = Some(PasteFailure::Clipboard);
            return None;
        };
        if text.len() > MAX_PASTE_BYTES {
            self.failure = Some(PasteFailure::TooLong);
            return None;
        }
        let key = match PastedAnthropicKey::new(text) {
            Ok(key) => key,
            Err(reason) => {
                self.failure = Some(PasteFailure::Rejected(reason));
                return None;
            }
        };
        self.request = self.request.wrapping_add(1);
        let at = Attempt {
            session,
            request: self.request,
            deadline: now + PASTE_TIMEOUT,
        };
        if !send(ConnectionOp::BeginAnthropicKeyStage {
            request: at.request,
            len: key.len(),
        }) {
            self.failure = Some(PasteFailure::Offline);
            return None;
        }
        self.stage = Stage::Asked(at, key);
        Some(at)
    }

    pub fn planned(
        &mut self,
        session: u64,
        reply: &ControlToShell,
        now: Instant,
    ) -> Option<KeyWriteJob> {
        let ControlToShell::AnthropicKeyStage { request, result } = reply else {
            return None;
        };
        let Stage::Asked(at, _) = self.stage else {
            return None;
        };
        if at.session != session || at.request != *request || self.expire(at, now) {
            return None;
        }
        let Stage::Asked(_, key) = mem::take(&mut self.stage) else {
            return None;
        };
        match *result {
            Ok(plan) => {
                self.stage = Stage::Writing(at, plan.id);
                Some(KeyWriteJob {
                    at,
                    plan: plan.id,
                    permit: StagePermit::new(plan),
                    key,
                })
            }
            Err(error) => {
                self.stage_failed(error);
                None
            }
        }
    }

    pub fn written(
        &mut self,
        result: &KeyWriteResult,
        now: Instant,
        send: impl FnOnce(ConnectionOp) -> bool,
    ) {
        let KeyWriteResult {
            at,
            plan,
            ref result,
        } = *result;
        if !matches!(self.stage, Stage::Writing(writing, expected) if writing == at && expected == plan)
            || self.expire(at, now)
        {
            return;
        }
        self.stage = Stage::Idle;
        if result.is_err() {
            self.failure = Some(PasteFailure::Unsaved);
        } else if send(ConnectionOp::AnthropicKeyStaged { plan }) {
            self.stage = Stage::Saving(at, plan);
        } else {
            self.failure = Some(PasteFailure::Offline);
        }
    }

    pub fn done(&mut self, session: u64, reply: &ControlToShell, now: Instant) {
        let ControlToShell::AnthropicKeyStageDone { plan, result } = reply else {
            return;
        };
        let Stage::Saving(at, expected) = self.stage else {
            return;
        };
        if at.session != session || expected != *plan || self.expire(at, now) {
            return;
        }
        self.stage = Stage::Idle;
        match *result {
            Ok(()) => {
                self.saved = true;
                self.view = Some(KeyView::Checking);
            }
            Err(error) => self.stage_failed(error),
        }
    }

    fn stage_failed(&mut self, error: KeyStageFailure) {
        self.failure = Some(match error {
            KeyStageFailure::Rejected(reason) => PasteFailure::Rejected(reason),
            KeyStageFailure::Unavailable
            | KeyStageFailure::InvalidLength
            | KeyStageFailure::Changed => PasteFailure::Unsaved,
        });
    }

    pub fn expire(&mut self, at: Attempt, now: Instant) -> bool {
        if now < at.deadline || self.stage.attempt() != Some(at) {
            return false;
        }
        self.failure = Some(if matches!(self.stage, Stage::Saving(..)) {
            PasteFailure::Unconfirmed
        } else {
            PasteFailure::Unsaved
        });
        self.stage = Stage::Idle;
        true
    }

    pub fn check_available(&self) -> bool {
        self.removal() == Removal::Retry
            || (self.available()
                && (self.view == Some(KeyView::Unconfirmed)
                    || self.failure == Some(PasteFailure::Unconfirmed)))
    }

    pub fn check(&mut self, send: impl FnOnce(ConnectionOp) -> bool) {
        if !self.check_available() {
            return;
        }
        if send(ConnectionOp::CheckAnthropicKey) {
            self.failure = None;
            self.receive(KeyView::Checking);
        } else if self.failure != Some(PasteFailure::Unconfirmed) {
            self.failure = Some(PasteFailure::Offline);
        }
    }

    pub fn remove_revision(&self) -> Option<u64> {
        (self.can_paste() && self.view != Some(KeyView::Missing)).then_some(self.revision)
    }

    pub fn remove(&mut self, revision: u64, send: impl FnOnce(ConnectionOp) -> bool) {
        if self.remove_revision() != Some(revision) {
            return;
        }
        self.revision = self.revision.wrapping_add(1);
        if send(ConnectionOp::RemoveAnthropicKey) {
            self.failure = None;
            self.removing = true;
            self.receive(KeyView::Checking);
        } else {
            self.failure = Some(PasteFailure::Offline);
        }
    }
}

#[cfg(test)]
#[path = "key_paste_tests.rs"]
mod tests;
