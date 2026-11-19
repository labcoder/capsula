//! One explicit paste through the brain's authority and the shell's credential writer.

use std::fmt;
use std::mem;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use kumochi_protocol::message::{
    ConnectionOp, ControlToShell, KeyRecoveryRead, KeyRejection, KeyRemovalFailure,
    KeyStageFailure, KeyStageId, KeyView,
};
use zeroize::Zeroizing;

use crate::platform::clipboard::{ClipboardRefusal, KeyClipboardResult, MAX_KEY_CLIPBOARD_BYTES};
use crate::platform::credential::{PastedAnthropicKey, StageError, StagePermit, write_key_stage};

const PASTE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub session: u64,
    pub request: u64,
    pub deadline: Instant,
}

pub struct ClipboardAttempt {
    at: Attempt,
    live: Weak<()>,
}

impl ClipboardAttempt {
    pub fn attempt(&self) -> Attempt {
        self.at
    }

    pub fn is_current(&self) -> bool {
        self.live.strong_count() != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteFailure {
    Clipboard,
    TooLong,
    Rejected(KeyRejection),
    Unsaved,
    Unconfirmed,
    Offline,
    RemovalChanged,
}

#[derive(Default)]
enum Stage {
    #[default]
    Idle,
    Reading {
        at: Attempt,
        _live: Arc<()>,
    },
    Asked(Attempt, PastedAnthropicKey),
    Writing(Attempt, KeyStageId),
    Saving(Attempt, KeyStageId),
}

impl Stage {
    fn attempt(&self) -> Option<Attempt> {
        match self {
            Self::Idle => None,
            Self::Reading { at, .. }
            | Self::Asked(at, _)
            | Self::Writing(at, _)
            | Self::Saving(at, _) => Some(*at),
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
    #[cfg(any(test, feature = "test-support"))]
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

enum RemovalAttempt {
    Waiting { request: u64, revision: u64 },
    Retry { revision: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemovalConfirmation {
    authority_revision: u64,
    epoch: u64,
}

#[derive(Default)]
pub struct KeyPaste {
    stage: Stage,
    request: u64,
    confirmation_epoch: u64,
    authority_revision: Option<u64>,
    view: Option<KeyView>,
    saved: bool,
    removal: Option<RemovalAttempt>,
    reconciliation: Option<u64>,
    recovery_read: bool,
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
    pub fn receive(&mut self, view: KeyView, authority_revision: Option<u64>) -> bool {
        let authority_changed = self.authority_revision != authority_revision;
        let changed = self.view != Some(view) || authority_changed;
        self.recovery_read &= !changed && view == KeyView::Checking;
        self.view = Some(view);
        self.authority_revision = authority_revision;
        if matches!(view, KeyView::Missing) {
            self.saved = false;
        } else if matches!(view, KeyView::Ready | KeyView::Rejected(_)) {
            self.saved = true;
        }
        if authority_changed {
            self.confirmation_epoch = self.confirmation_epoch.wrapping_add(1);
        }
        changed
    }

    pub fn disconnected(&mut self) {
        let unknown = self.failure == Some(PasteFailure::Unconfirmed)
            || matches!(self.stage, Stage::Saving(..));
        *self = Self {
            request: self.request,
            confirmation_epoch: self.confirmation_epoch.wrapping_add(1),
            failure: unknown.then_some(PasteFailure::Unconfirmed),
            ..Self::default()
        };
    }

    pub fn view(&self) -> Option<KeyView> {
        self.view
    }
    pub fn saved(&self) -> bool {
        self.saved && self.failure != Some(PasteFailure::Unconfirmed)
    }
    pub fn failure(&self) -> Option<PasteFailure> {
        self.failure
    }
    pub fn removal(&self) -> Removal {
        match self.removal {
            None => Removal::None,
            Some(RemovalAttempt::Waiting { .. }) => Removal::Waiting,
            Some(RemovalAttempt::Retry { .. }) => Removal::Retry,
        }
    }
    pub fn saving(&self) -> bool {
        self.stage.attempt().is_some()
    }

    pub fn can_paste(&self) -> bool {
        self.available() && self.failure != Some(PasteFailure::Unconfirmed)
    }

    fn available(&self) -> bool {
        self.view
            .is_some_and(|view| view != KeyView::Checking || self.recovery_read)
            && !self.saving()
            && self.removal.is_none()
    }

    pub fn start_read(&mut self, session: u64, now: Instant) -> Option<ClipboardAttempt> {
        if !self.can_paste() {
            return None;
        }
        self.confirmation_epoch = self.confirmation_epoch.wrapping_add(1);
        self.failure = None;
        self.request = self.request.wrapping_add(1);
        let at = Attempt {
            session,
            request: self.request,
            deadline: now + PASTE_TIMEOUT,
        };
        let live = Arc::new(());
        let pending = ClipboardAttempt {
            at,
            live: Arc::downgrade(&live),
        };
        self.stage = Stage::Reading { at, _live: live };
        Some(pending)
    }

    pub fn clipboard_read(
        &mut self,
        at: Attempt,
        result: KeyClipboardResult,
        now: Instant,
        send: impl FnOnce(ConnectionOp) -> bool,
    ) -> bool {
        if !matches!(self.stage, Stage::Reading { at: reading, .. } if reading == at)
            || self.expire(at, now)
        {
            return false;
        }
        self.stage = Stage::Idle;
        let text = match result {
            Ok(text) if !text.is_empty() => text,
            Err(ClipboardRefusal::TooLong) => {
                self.failure = Some(PasteFailure::TooLong);
                return false;
            }
            _ => {
                self.failure = Some(PasteFailure::Clipboard);
                return false;
            }
        };
        if text.len() > MAX_KEY_CLIPBOARD_BYTES {
            self.failure = Some(PasteFailure::TooLong);
            return false;
        }
        let key = match PastedAnthropicKey::new(text) {
            Ok(key) => key,
            Err(reason) => {
                self.failure = Some(PasteFailure::Rejected(reason));
                return false;
            }
        };
        if !send(ConnectionOp::BeginAnthropicKeyStage {
            request: at.request,
            len: key.len(),
        }) {
            self.failure = Some(PasteFailure::Offline);
            return false;
        }
        self.stage = Stage::Asked(at, key);
        true
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn begin(
        &mut self,
        text: Option<Zeroizing<String>>,
        session: u64,
        now: Instant,
        send: impl FnOnce(ConnectionOp) -> bool,
    ) -> Option<Attempt> {
        let at = self.start_read(session, now)?.attempt();
        self.clipboard_read(at, text.ok_or(ClipboardRefusal::Invalid), now, send)
            .then_some(at)
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
                self.recovery_read = false;
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
        self.failure = Some(match self.stage {
            Stage::Reading { .. } => PasteFailure::Clipboard,
            Stage::Saving(..) => PasteFailure::Unconfirmed,
            _ => PasteFailure::Unsaved,
        });
        self.stage = Stage::Idle;
        true
    }

    pub fn check_available(&self) -> bool {
        if self.failure == Some(PasteFailure::Unconfirmed) {
            return self.view.is_some() && self.reconciliation.is_none();
        }
        self.removal() == Removal::Retry
            || (self.available() && self.view == Some(KeyView::Unconfirmed))
    }

    pub fn check(&mut self, send: impl FnOnce(ConnectionOp) -> bool) {
        if !self.check_available() {
            return;
        }
        if let Some(RemovalAttempt::Retry { revision }) = self.removal {
            self.send_remove(revision, send);
            return;
        }
        if self.failure == Some(PasteFailure::Unconfirmed) {
            self.request = self.request.wrapping_add(1);
            if send(ConnectionOp::ReconcileAnthropicKey {
                request: self.request,
            }) {
                self.reconciliation = Some(self.request);
            }
            return;
        }
        if send(ConnectionOp::CheckAnthropicKey) {
            self.failure = None;
            self.receive(KeyView::Checking, self.authority_revision);
        } else {
            self.failure = Some(PasteFailure::Offline);
        }
    }

    pub fn reconciled(&mut self, reply: &ControlToShell) {
        let ControlToShell::AnthropicKeyReconciled { request, read } = *reply else {
            return;
        };
        if self.reconciliation != Some(request) {
            return;
        }
        self.reconciliation = None;
        let Some(read) = read else {
            return;
        };
        self.failure = None;
        self.saved = false;
        match read {
            KeyRecoveryRead::Missing => {
                self.receive(KeyView::Missing, None);
            }
            KeyRecoveryRead::Present {
                revision,
                rejection,
            } => {
                self.receive(
                    rejection.map_or(KeyView::Checking, KeyView::Rejected),
                    Some(revision),
                );
                self.recovery_read = rejection.is_none();
            }
        }
    }

    pub fn removal_confirmation(&self) -> Option<RemovalConfirmation> {
        if !self.can_paste() || self.view == Some(KeyView::Missing) {
            return None;
        }
        Some(RemovalConfirmation {
            authority_revision: self.authority_revision?,
            epoch: self.confirmation_epoch,
        })
    }

    pub fn remove(
        &mut self,
        confirmation: RemovalConfirmation,
        send: impl FnOnce(ConnectionOp) -> bool,
    ) {
        if self.removal_confirmation() != Some(confirmation) {
            return;
        }
        self.confirmation_epoch = self.confirmation_epoch.wrapping_add(1);
        self.send_remove(confirmation.authority_revision, send);
    }

    fn send_remove(&mut self, revision: u64, send: impl FnOnce(ConnectionOp) -> bool) {
        self.request = self.request.wrapping_add(1);
        let request = self.request;
        if send(ConnectionOp::RemoveAnthropicKey { revision, request }) {
            self.failure = None;
            self.removal = Some(RemovalAttempt::Waiting { request, revision });
            self.receive(KeyView::Checking, self.authority_revision);
        } else {
            self.failure = Some(PasteFailure::Offline);
        }
    }

    pub fn removed(&mut self, reply: &ControlToShell) {
        let ControlToShell::AnthropicKeyRemoved { request, result } = *reply else {
            return;
        };
        let Some(RemovalAttempt::Waiting {
            request: expected,
            revision,
        }) = self.removal
        else {
            return;
        };
        if request != expected {
            return;
        }
        match result {
            Ok(()) => {
                self.removal = None;
                self.receive(KeyView::Missing, None);
            }
            Err(KeyRemovalFailure::Changed) => {
                self.removal = None;
                self.saved = false;
                self.failure = Some(PasteFailure::RemovalChanged);
            }
            Err(KeyRemovalFailure::Unconfirmed) => {
                self.removal = Some(RemovalAttempt::Retry { revision });
            }
        }
    }
}

#[cfg(test)]
#[path = "key_paste_tests.rs"]
mod tests;
