//! Anthropic connection work shares the account worker without entering its token state machine.

use std::fmt;
use std::sync::Arc;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{
    ClaudeCodeView, ConnectionKind, ConnectionOp, ConnectionsView, ControlToShell, DataToShell,
    KeyRecoveryRead, KeyRemovalFailure, KeyStageFailure, KeyStageId, KeyStagePlan, KeyView, RunId,
};
use tokio::task::JoinHandle;

use crate::connections::anthropic::{
    KeyError, KeyOp, KeyResult,
    fence::Fence,
    key::AnthropicKey,
    models::{CatalogModel, CheckError},
    route::{Transport, Verified},
};
use crate::context::ConnectionTag;
use crate::coordinator::{Availability, Input, Ready, SendKey, model_unavailable};
use crate::engines::anthropic::live::Live;
use crate::engines::{Engine, RunRequest};

use super::account::{Accounts, Event, Work};
use super::{Brain, Full, Link};

mod question;

pub(crate) enum ConnectionEvent {
    Op(ConnectionOp),
    StagePlan {
        epoch: u64,
        request: u64,
        result: Result<(u64, KeyStagePlan), KeyStageFailure>,
    },
    StageDone {
        epoch: u64,
        generation: u64,
        plan: KeyStageId,
        result: Result<(u64, Secret<AnthropicKey>), KeyStageFailure>,
    },
    Loaded {
        generation: u64,
        result: KeyResult,
    },
    Checked {
        generation: u64,
        result: Result<Vec<CatalogModel>, CheckError>,
    },
    /// A Messages post on the authority tagged `tag` got a 401; its fence is already closed.
    Refused {
        tag: ConnectionTag,
    },
}

pub(super) enum StageWork {
    Begin {
        epoch: u64,
        request: u64,
        len: u16,
    },
    Adopt {
        epoch: u64,
        generation: u64,
        plan: KeyStageId,
    },
}

impl fmt::Debug for StageWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Begin { .. } => "StageWork::Begin",
            Self::Adopt { .. } => "StageWork::Adopt",
        })
    }
}

fn stage_error(error: &KeyError) -> KeyStageFailure {
    match error {
        KeyError::Rejected(reason) => KeyStageFailure::Rejected(*reason),
        KeyError::Changed => KeyStageFailure::Changed,
        KeyError::Vault(_) => KeyStageFailure::Unavailable,
    }
}

impl StageWork {
    pub(super) fn run<S: crate::platform::credential::CredentialStore>(
        self,
        worker: &mut crate::connections::account::Worker<S>,
    ) -> ConnectionEvent {
        match self {
            Self::Begin {
                epoch,
                request,
                len,
            } => ConnectionEvent::StagePlan {
                epoch,
                request,
                result: worker
                    .begin_key_paste(usize::from(len))
                    .map(|plan| {
                        (
                            plan.base,
                            KeyStagePlan {
                                id: plan.generation.stage_id(),
                                len: u16::try_from(plan.bytes).expect("a paste length fits u16"),
                            },
                        )
                    })
                    .map_err(|error| {
                        if matches!(
                            error,
                            KeyError::Vault(crate::connections::error::VaultError::Bounds)
                        ) {
                            KeyStageFailure::InvalidLength
                        } else {
                            stage_error(&error)
                        }
                    }),
            },
            Self::Adopt {
                epoch,
                generation,
                plan,
            } => ConnectionEvent::StageDone {
                epoch,
                generation,
                plan,
                result: worker
                    .adopt_key_paste(crate::connections::authority::Generation::from_stage_id(
                        plan,
                    ))
                    .map_err(|error| match error {
                        crate::connections::vault::staging::PasteError::Changed => {
                            KeyStageFailure::Changed
                        }
                        crate::connections::vault::staging::PasteError::Key(error) => {
                            stage_error(&error)
                        }
                    }),
            },
        }
    }
}

impl fmt::Debug for ConnectionEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConnectionEvent")
    }
}

pub(super) struct Connections {
    generation: u64,
    key: Option<Secret<AnthropicKey>>,
    transport: Option<Transport>,
    verified: Option<Arc<Verified>>,
    checking: Option<Fence>,
    adopting: Option<(u64, Option<u64>)>,
    recovering: Option<u64>,
    #[cfg(test)]
    before_key_mutation: Option<Box<dyn Fn() + Send + Sync>>,
    view: ConnectionsView,
    models: Vec<CatalogModel>,
    /// The current key's last Check verified it, so `models`' picture facts are current. Every
    /// inconclusive path clears it; only a verifying Check of the current generation sets it.
    // shortcut: after an inconclusive Check, pictures are omitted, with a truthful notice, until
    // the next conclusive Check.
    fresh: bool,
    unsent: bool,
    read: Option<JoinHandle<()>>,
    loading: bool,
    /// A Remove is admitted but hasn't committed (design §5): every key op retries it.
    // shortcut: the pending fence is process-local, so a restart before the removal commits can
    // reload the key; a durable pending intent is needed only if removal must survive that exit.
    removing: Option<u64>,
    remove_request: Option<(u64, u64)>,
    check_request: Option<(u64, u64)>,
    stage_link: Option<u64>,
    stage_request: Option<u64>,
    stage_admission: Option<KeyStageId>,
}

impl Default for Connections {
    fn default() -> Self {
        Self {
            generation: 0,
            key: None,
            transport: None,
            verified: None,
            checking: None,
            adopting: None,
            recovering: None,
            #[cfg(test)]
            before_key_mutation: None,
            view: ConnectionsView {
                anthropic: KeyView::Missing,
                anthropic_revision: None,
                claude_code: ClaudeCodeView::NotFound,
            },
            models: Vec::new(),
            fresh: false,
            unsent: false,
            read: None,
            loading: false,
            removing: None,
            remove_request: None,
            check_request: None,
            stage_link: None,
            stage_request: None,
            stage_admission: None,
        }
    }
}

impl fmt::Debug for Connections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connections")
            .field("generation", &self.generation)
            .field("view", &self.view)
            .field("models", &self.models.len())
            .finish_non_exhaustive()
    }
}

impl Connections {
    #[cfg(test)]
    pub(super) fn use_test_transport(&mut self, transport: Transport) {
        self.transport = Some(transport);
    }

    pub(super) fn configure_transport(&mut self) {
        if self.transport.is_none() {
            self.transport = Transport::native();
        }
    }

    fn route_input(&self) -> Option<Input> {
        self.transport.as_ref()?;
        let availability = self.verified.as_ref().map_or_else(
            || Availability::Unavailable(model_unavailable()),
            |verified| {
                Availability::Ready(Ready {
                    connection: verified.tag,
                    models: self.models.iter().map(|model| model.info.clone()).collect(),
                    #[cfg(any(test, debug_assertions))]
                    trusted_draw: false,
                    images: self
                        .models
                        .iter()
                        .filter(|model| {
                            self.fresh && verified.fence.open() && model.capabilities.images
                        })
                        .map(|model| model.info.slug.clone())
                        .collect(),
                })
            },
        );
        Some(Input::Engines(ConnectionKind::AnthropicKey, availability))
    }

    fn retire(&mut self) {
        self.fresh = false;
        if let Some(verified) = self.verified.take() {
            verified.fence.revoke();
        }
        self.cancel();
    }

    fn verify(&mut self) -> bool {
        if self.transport.is_none() {
            return false;
        }
        let key = self.key.take();
        if let Some(verified) = &self.verified {
            // A post already found it refused; the account hears that next and retires it.
            return verified.fence.open();
        }
        let (Some(revision), Some(key)) = (self.view.anthropic_revision, key) else {
            return false;
        };
        self.verified = Verified::new(revision, key).map(Arc::new);
        self.verified.is_some()
    }

    pub(super) fn stage_session(&mut self, epoch: Option<u64>) {
        self.stage_link = epoch;
        self.stage_request = None;
        self.stage_admission = None;
        self.check_request = None;
    }
    pub(super) fn cancel(&mut self) {
        if let Some(checking) = self.checking.take() {
            checking.revoke();
        }
        if let Some(read) = self.read.take() {
            read.abort();
        }
    }

    pub(super) fn close(&mut self) {
        if self.transport.is_some() {
            self.retire();
            self.advance();
            self.loading = false;
        } else {
            self.cancel();
        }
    }

    /// The key's half of the view; the runtime puts it together with Claude Code's.
    pub(super) fn anthropic(&self) -> KeyView {
        self.view.anthropic
    }

    pub(super) fn anthropic_revision(&self) -> Option<u64> {
        self.view.anthropic_revision
    }

    pub(super) fn connected(&mut self) -> ControlToShell {
        self.unsent |= self.view.anthropic == KeyView::Ready;
        ControlToShell::Connections(self.view)
    }

    pub(super) fn models_unsent(&mut self) -> Option<DataToShell> {
        std::mem::take(&mut self.unsent).then(|| DataToShell::Models {
            connection: ConnectionKind::AnthropicKey,
            models: self.models.iter().map(|model| model.info.clone()).collect(),
        })
    }

    fn advance(&mut self) -> u64 {
        self.cancel();
        self.generation = self
            .generation
            .checked_add(1)
            .expect("key generation cannot exhaust u64 in one process");
        self.key = None;
        self.adopting = None;
        self.recovering = None;
        if self.verified.is_none() {
            self.unsent |= !self.models.is_empty() || self.view.anthropic == KeyView::Ready;
            self.models.clear();
        }
        self.generation
    }
}

impl Drop for Connections {
    fn drop(&mut self) {
        self.retire();
    }
}

impl Accounts {
    fn send_key_work(&self, work: Work) -> bool {
        let Some(jobs) = &self.jobs else {
            return false;
        };
        #[cfg(test)]
        if matches!(
            work,
            Work::Stage(StageWork::Adopt { .. })
                | Work::Key {
                    op: KeyOp::Remove(_),
                    ..
                }
        ) && let Some(observe) = &self.connections.before_key_mutation
        {
            observe();
        }
        jobs.send(work).is_ok()
    }

    pub(super) fn stage_op(&mut self, epoch: u64, op: ConnectionOp) -> Vec<ControlToShell> {
        let work = match op {
            ConnectionOp::BeginAnthropicKeyStage { request, len } => {
                if self.connections.removing.is_some()
                    || self.connections.check_request.is_some()
                    || self.connections.stage_link != Some(epoch)
                {
                    return vec![ControlToShell::AnthropicKeyStage {
                        request,
                        result: Err(KeyStageFailure::Unavailable),
                    }];
                }
                self.connections.stage_admission = None;
                self.connections.stage_request = Some(request);
                StageWork::Begin {
                    epoch,
                    request,
                    len,
                }
            }
            ConnectionOp::AnthropicKeyStaged { plan } => {
                if self.connections.removing.is_some()
                    || self.connections.adopting.is_some()
                    || self.connections.check_request.is_some()
                    || self.connections.stage_link != Some(epoch)
                    || self.connections.stage_admission != Some(plan)
                {
                    return vec![ControlToShell::AnthropicKeyStageDone {
                        plan,
                        result: Err(KeyStageFailure::Changed),
                    }];
                }
                self.connections.stage_admission = None;
                if self.connections.transport.is_some() {
                    let base = self
                        .connections
                        .verified
                        .as_ref()
                        .map(|key| key.revision)
                        .or(self.connections.view.anthropic_revision);
                    self.connections.retire();
                    let generation = self.connections.advance();
                    self.connections.adopting = Some((generation, base));
                    self.connections.view.anthropic = KeyView::Checking;
                }
                StageWork::Adopt {
                    epoch,
                    generation: self.connections.generation,
                    plan,
                }
            }
            _ => return Vec::new(),
        };
        if !self.send_key_work(Work::Stage(work)) {
            return match op {
                ConnectionOp::BeginAnthropicKeyStage { request, .. } => {
                    vec![ControlToShell::AnthropicKeyStage {
                        request,
                        result: Err(KeyStageFailure::Unavailable),
                    }]
                }
                ConnectionOp::AnthropicKeyStaged { plan } => {
                    let mut frames = Vec::new();
                    if self.connections.adopting.take().is_some() {
                        self.connections.view.anthropic = KeyView::Unconfirmed;
                        frames.push(ControlToShell::Connections(self.connections.view));
                    }
                    frames.push(ControlToShell::AnthropicKeyStageDone {
                        plan,
                        result: Err(KeyStageFailure::Unavailable),
                    });
                    frames
                }
                _ => Vec::new(),
            };
        }
        Vec::new()
    }
    pub(super) fn start_key(&mut self, result: KeyResult) {
        if matches!(result.1, Ok(None)) {
            return;
        }
        let generation = self.connections.advance();
        self.connections.loading = true;
        self.connections.view.anthropic = KeyView::Checking;
        self.connections.view.anthropic_revision = result.0;
        self.reads_tx
            .send(Event::Connection(ConnectionEvent::Loaded {
                generation,
                result,
            }))
            .ok();
    }

    pub(super) fn load_key(&mut self, op: KeyOp) {
        if matches!(op, KeyOp::Remove(_)) {
            self.connections.retire();
        }
        let generation = self.connections.advance();
        self.connections.view.anthropic = KeyView::Checking;
        self.connections.view.anthropic_revision = None;
        self.connections.loading = true;
        let work = Work::Key { generation, op };
        if !self.send_key_work(work) {
            self.connections.view.anthropic = KeyView::Unconfirmed;
            self.connections.loading = false;
        }
    }

    fn removal_reply(&mut self, result: Result<(), KeyRemovalFailure>) -> Option<ControlToShell> {
        let (epoch, request) = self.connections.remove_request.take()?;
        (self.connections.stage_link == Some(epoch))
            .then_some(ControlToShell::AnthropicKeyRemoved { request, result })
    }

    fn recovery_check(&mut self, request: u64) -> Vec<ControlToShell> {
        if self.connections.stage_link.is_none() {
            return Vec::new();
        }
        let refused = ControlToShell::AnthropicKeyReconciled {
            request,
            read: None,
        };
        if self.connections.removing.is_some() || self.connections.check_request.is_some() {
            return vec![refused];
        }
        self.load_key(KeyOp::Load);
        let snapshot = ControlToShell::Connections(self.connections.view);
        if self.connections.loading {
            self.connections.check_request = Some((self.connections.generation, request));
            vec![snapshot]
        } else {
            vec![snapshot, refused]
        }
    }

    fn take_check_request(&mut self, generation: u64) -> Option<u64> {
        self.connections
            .check_request
            .take_if(|(expected, _)| *expected == generation)
            .map(|(_, request)| request)
    }

    fn recovery_read(&mut self, generation: u64, result: &KeyResult) -> Option<ControlToShell> {
        let request = self.take_check_request(generation)?;
        let read = match (result.0, &result.1) {
            (_, Ok(None)) => Some(KeyRecoveryRead::Missing),
            (Some(revision), Ok(Some(_))) => Some(KeyRecoveryRead::Present {
                revision,
                rejection: None,
            }),
            (Some(revision), Err(KeyError::Rejected(reason))) => Some(KeyRecoveryRead::Present {
                revision,
                rejection: Some(*reason),
            }),
            _ => None,
        };
        Some(ControlToShell::AnthropicKeyReconciled { request, read })
    }

    fn stage_done(
        &mut self,
        epoch: u64,
        generation: u64,
        plan: KeyStageId,
        result: Result<(u64, Secret<AnthropicKey>), KeyStageFailure>,
    ) -> Vec<ControlToShell> {
        let before = self.connections.view;
        let adopted = self
            .connections
            .adopting
            .take_if(|(expected, _)| *expected == generation);
        let outcome = match result {
            Ok((revision, key)) => {
                if generation == self.connections.generation {
                    self.start_key((Some(revision), Ok(Some(key))));
                }
                Ok(())
            }
            Err(error) => {
                if let Some((_, base)) = adopted {
                    self.connections.view.anthropic = KeyView::Unconfirmed;
                    if self.connections.removing.is_none()
                        && let Some(base) = base
                    {
                        self.load_key(KeyOp::Load);
                        self.connections.recovering = Some(base);
                    }
                }
                Err(error)
            }
        };
        let mut frames = Vec::new();
        if outcome.is_err() && before != self.connections.view {
            frames.push(ControlToShell::Connections(self.connections.view));
        }
        if self.connections.stage_link == Some(epoch) {
            frames.push(ControlToShell::AnthropicKeyStageDone {
                plan,
                result: outcome,
            });
        }
        frames
    }

    fn key_loaded(
        &mut self,
        generation: u64,
        (revision, result): KeyResult,
    ) -> Option<ControlToShell> {
        let mut removal = None;
        self.connections.loading = false;
        if self.connections.removing.is_some() {
            if matches!(result, Err(KeyError::Changed)) {
                self.connections.removing = None;
                self.load_key(KeyOp::Load);
                return self.removal_reply(Err(KeyRemovalFailure::Changed));
            }
            self.connections.stage_request = None;
            self.connections.stage_admission = None;
            if matches!(result, Ok(None)) {
                self.connections.removing = None;
                removal = self.removal_reply(Ok(()));
            } else {
                removal = self.removal_reply(Err(KeyRemovalFailure::Unconfirmed));
            }
        }
        self.connections.view.anthropic_revision = if matches!(result, Ok(None)) {
            None
        } else {
            revision
        };
        if let Some(old) = &self.connections.verified
            && (revision != Some(old.revision) || !matches!(result, Ok(Some(_))))
        {
            self.connections.retire();
            self.connections.unsent = true;
            self.connections.models.clear();
        }
        if self.connections.transport.is_some()
            && (self.connections.removing.is_some()
                || (revision.is_none() && matches!(result, Ok(Some(_))))
                || self
                    .connections
                    .recovering
                    .take()
                    .is_some_and(|base| revision != Some(base)))
        {
            self.connections.view.anthropic = KeyView::Unconfirmed;
            return removal;
        }
        match result {
            Ok(Some(key)) => {
                let Some(transport) = self.connections.transport.clone() else {
                    self.connections.view.anthropic = KeyView::Unconfirmed;
                    return removal;
                };
                self.connections.key = Some(key.clone());
                let checking = Fence::default();
                self.connections.checking = Some(checking.clone());
                let events = self.reads_tx.clone();
                self.connections.read = Some(tokio::spawn(async move {
                    let result = transport.catalog(&checking, &key).await;
                    events
                        .send(Event::Connection(ConnectionEvent::Checked {
                            generation,
                            result,
                        }))
                        .ok();
                }));
            }
            Ok(None) => self.connections.view.anthropic = KeyView::Missing,
            Err(KeyError::Rejected(reason)) => {
                self.connections.view.anthropic = KeyView::Rejected(reason);
            }
            Err(KeyError::Vault(_) | KeyError::Changed) => {
                self.connections.view.anthropic = KeyView::Unconfirmed;
            }
        }
        removal
    }

    fn key_op(&mut self, op: ConnectionOp) -> Vec<ControlToShell> {
        if let ConnectionOp::ReconcileAnthropicKey { request } = op {
            return self.recovery_check(request);
        }
        let in_flight = self.connections.removing.is_some() && self.connections.loading;
        match op {
            ConnectionOp::CheckAnthropicKey => {}
            ConnectionOp::RemoveAnthropicKey { revision, request } => {
                if self
                    .connections
                    .removing
                    .is_some_and(|pending| pending != revision)
                {
                    return vec![ControlToShell::AnthropicKeyRemoved {
                        request,
                        result: Err(KeyRemovalFailure::Changed),
                    }];
                }
                self.connections.removing.get_or_insert(revision);
                self.connections.remove_request =
                    self.connections.stage_link.map(|epoch| (epoch, request));
            }
            _ => return Vec::new(),
        }
        if in_flight {
            return Vec::new();
        }
        let abandoned = self
            .take_check_request(self.connections.generation)
            .map(|request| ControlToShell::AnthropicKeyReconciled {
                request,
                read: None,
            });
        self.load_key(self.connections.removing.map_or(KeyOp::Load, KeyOp::Remove));
        let removal = if self.connections.loading {
            None
        } else {
            self.removal_reply(Err(KeyRemovalFailure::Unconfirmed))
        };
        std::iter::once(ControlToShell::Connections(self.connections.view))
            .chain(removal)
            .chain(abandoned)
            .collect()
    }

    #[expect(clippy::too_many_lines, reason = "one arm per connection event")]
    pub(super) fn connection_event(&mut self, event: ConnectionEvent) -> Vec<ControlToShell> {
        let reply = match event {
            ConnectionEvent::StagePlan {
                epoch,
                request,
                result,
            } => {
                if self.connections.stage_link != Some(epoch)
                    || self.connections.stage_request != Some(request)
                {
                    return Vec::new();
                }
                self.connections.stage_request = None;
                if matches!(result, Ok((base, _)) if self.connections.removing == Some(base)) {
                    return vec![ControlToShell::AnthropicKeyStage {
                        request,
                        result: Err(KeyStageFailure::Unavailable),
                    }];
                }
                let result = result.map(|(_, plan)| plan);
                self.connections.stage_admission = result.as_ref().ok().map(|plan| plan.id);
                return vec![ControlToShell::AnthropicKeyStage { request, result }];
            }
            ConnectionEvent::StageDone {
                epoch,
                generation,
                plan,
                result,
            } => {
                return self.stage_done(epoch, generation, plan, result);
            }
            ConnectionEvent::Op(op) => return self.key_op(op),
            ConnectionEvent::Loaded { generation, result } => {
                if generation != self.connections.generation || !self.connections.loading {
                    return Vec::new();
                }
                let recovered = self.recovery_read(generation, &result);
                self.key_loaded(generation, result).or(recovered)
            }
            ConnectionEvent::Checked { generation, result } => {
                if generation != self.connections.generation || self.connections.read.is_none() {
                    return Vec::new();
                }
                self.connections.cancel();
                self.connections.fresh = false;
                match result {
                    Ok(models) => {
                        if self.connections.verify() {
                            self.connections.models = models;
                            self.connections.fresh = true;
                            self.connections.view.anthropic = KeyView::Ready;
                        } else {
                            self.connections.view.anthropic = KeyView::Unconfirmed;
                        }
                    }
                    Err(CheckError::Rejected(reason)) => {
                        if self.connections.transport.is_some() {
                            self.connections.retire();
                            self.connections.key = None;
                            self.connections.models.clear();
                        }
                        self.connections.view.anthropic = KeyView::Rejected(reason);
                    }
                    Err(CheckError::Unconfirmed) => {
                        if self.connections.transport.is_some() {
                            self.connections.key = None;
                        }
                        self.connections.view.anthropic = KeyView::Unconfirmed;
                    }
                }
                self.connections.unsent = true;
                None
            }
            ConnectionEvent::Refused { tag } => {
                let Some(revision) = self
                    .connections
                    .verified
                    .as_ref()
                    .filter(|verified| verified.tag == tag)
                    .map(|verified| verified.revision)
                else {
                    return Vec::new();
                };
                // A Check waiting on its load had cleared the revision; the saved key is unchanged.
                self.connections.view.anthropic_revision = Some(revision);
                let abandoned = self.connections.check_request.take().map(|(_, request)| {
                    ControlToShell::AnthropicKeyReconciled {
                        request,
                        read: None,
                    }
                });
                self.connections.retire();
                self.connections.models.clear();
                // A Check already waiting on its load or its catalog read can't bring the key back.
                self.connections.advance();
                self.connections.loading = false;
                self.connections.view.anthropic = KeyView::Unconfirmed;
                self.connections.unsent = true;
                abandoned
            }
        };
        std::iter::once(ControlToShell::Connections(self.connections.view))
            .chain(reply)
            .collect()
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(super) use tests::with_empty_catalog;

impl Brain {
    pub(super) fn key_engine(&mut self, request: &RunRequest) -> Option<Box<dyn Engine>> {
        let transport = self.account.connections.transport.clone()?;
        let (tag, motion) = self.coordinator.key_motion(request)?;
        let authority = self.account.connections.verified.as_ref()?;
        if authority.tag != tag {
            return None;
        }
        let authority = Arc::clone(authority);
        let allowance = self.key_allowance.of(request.run)?;
        let account = self.account.reads_tx.clone();
        // Unbounded: one event per refused post, which can't follow on a closed fence, and a send
        // to an account that has gone only returns an error.
        let transport = transport.telling(Arc::new(move |tag| {
            account
                .send(Event::Connection(ConnectionEvent::Refused { tag }))
                .ok();
        }));
        let live = if let SendKey::Build(key) = request.key {
            if let Some((_, old)) = self.key_round.take_if(|(build, _)| build.run != key.run) {
                old.retire();
            }
            let (owned, live) = self
                .key_round
                .get_or_insert_with(|| (key, Arc::new(Live::new(allowance))));
            *owned = key;
            Arc::clone(live)
        } else {
            Arc::new(Live::new(allowance))
        };
        Some(Box::new(question::Question {
            transport,
            authority,
            live,
            tag,
            motion,
        }))
    }

    pub(super) fn retire_key_run(&mut self, run: RunId) {
        if let Some((_, live)) = self.key_round.take_if(|(held, _)| held.run == run) {
            live.retire();
        }
    }

    pub(super) fn key_route_changed(
        &mut self,
        link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        let Some(input) = self.account.connections.route_input() else {
            return Ok(());
        };
        self.hold_fence(&input);
        let effects = self
            .coordinator
            .handle(input)
            .expect("availability is never stale");
        self.apply(effects, link).map_err(|_| Full)
    }

    pub(super) fn connection_op(
        &mut self,
        op: ConnectionOp,
        link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        match op {
            ConnectionOp::ConnectClaudeCode
            | ConnectionOp::CancelClaudeSignIn
            | ConnectionOp::DisconnectClaudeCode
            | ConnectionOp::FindClaudeCode => self.claude_op(op, link),
            ConnectionOp::AnthropicKeyStaged { .. }
            | ConnectionOp::BeginAnthropicKeyStage { .. } => {
                let Some(link) = link else {
                    self.account.connections.stage_session(None);
                    return Ok(());
                };
                let frames = self.account.stage_op(self.epoch, op);
                self.key_route_changed(Some(link))?;
                frames
                    .iter()
                    .try_for_each(|frame| link.control(&self.composed(frame)))
            }
            ConnectionOp::ReconcileAnthropicKey { .. } if link.is_none() => Ok(()),
            ConnectionOp::CheckAnthropicKey
            | ConnectionOp::RemoveAnthropicKey { .. }
            | ConnectionOp::ReconcileAnthropicKey { .. } => {
                self.account_input(Event::Connection(ConnectionEvent::Op(op)), link)
            }
        }
    }
}
