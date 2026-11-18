//! Anthropic connection work shares the account worker without entering its token state machine.

use std::fmt;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{
    ClaudeCodeView, ConnectionKind, ConnectionOp, ConnectionsView, ControlToShell, DataToShell,
    KeyRemovalFailure, KeyStageFailure, KeyStageId, KeyStagePlan, KeyView,
};
use tokio::task::JoinHandle;
use tokio::time::{Duration, timeout};

use crate::connections::anthropic::{
    KeyError, KeyOp, KeyResult,
    key::AnthropicKey,
    models::{self, CatalogModel, CheckError},
};

use super::account::{Accounts, Event, Work};
use super::{Brain, Full, Link};

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
    view: ConnectionsView,
    models: Vec<CatalogModel>,
    unsent: bool,
    read: Option<JoinHandle<()>>,
    loading: bool,
    /// A Remove is admitted but hasn't committed (design §5): every key op retries it.
    // shortcut: the pending fence is process-local, so a restart before the removal commits can
    // reload the key; a durable pending intent is needed only if removal must survive that exit.
    removing: Option<u64>,
    remove_request: Option<(u64, u64)>,
    stage_link: Option<u64>,
    stage_request: Option<u64>,
    stage_admission: Option<KeyStageId>,
}

impl Default for Connections {
    fn default() -> Self {
        Self {
            generation: 0,
            key: None,
            view: ConnectionsView {
                anthropic: KeyView::Missing,
                anthropic_revision: None,
                claude_code: ClaudeCodeView::NotFound,
            },
            models: Vec::new(),
            unsent: false,
            read: None,
            loading: false,
            removing: None,
            remove_request: None,
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
    pub(super) fn stage_session(&mut self, epoch: Option<u64>) {
        self.stage_link = epoch;
        self.stage_request = None;
        self.stage_admission = None;
    }
    pub(super) fn cancel(&mut self) {
        if let Some(read) = self.read.take() {
            read.abort();
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
        self.unsent |= !self.models.is_empty() || self.view.anthropic == KeyView::Ready;
        self.models.clear();
        self.generation
    }
}

impl Drop for Connections {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl Accounts {
    pub(super) fn stage_op(&mut self, epoch: u64, op: ConnectionOp) -> Vec<ControlToShell> {
        let work = match op {
            ConnectionOp::BeginAnthropicKeyStage { request, len } => {
                if self.connections.removing.is_some() || self.connections.stage_link != Some(epoch)
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
                    || self.connections.stage_link != Some(epoch)
                    || self.connections.stage_admission != Some(plan)
                {
                    return vec![ControlToShell::AnthropicKeyStageDone {
                        plan,
                        result: Err(KeyStageFailure::Changed),
                    }];
                }
                self.connections.stage_admission = None;
                StageWork::Adopt {
                    epoch,
                    generation: self.connections.generation,
                    plan,
                }
            }
            _ => return Vec::new(),
        };
        if self
            .jobs
            .as_ref()
            .is_none_or(|jobs| jobs.send(Work::Stage(work)).is_err())
        {
            return match op {
                ConnectionOp::BeginAnthropicKeyStage { request, .. } => {
                    vec![ControlToShell::AnthropicKeyStage {
                        request,
                        result: Err(KeyStageFailure::Unavailable),
                    }]
                }
                ConnectionOp::AnthropicKeyStaged { plan } => {
                    vec![ControlToShell::AnthropicKeyStageDone {
                        plan,
                        result: Err(KeyStageFailure::Unavailable),
                    }]
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
        let generation = self.connections.advance();
        self.connections.view.anthropic = KeyView::Checking;
        self.connections.view.anthropic_revision = None;
        self.connections.loading = true;
        let work = Work::Key { generation, op };
        if self
            .jobs
            .as_ref()
            .is_none_or(|jobs| jobs.send(work).is_err())
        {
            self.connections.view.anthropic = KeyView::Unconfirmed;
            self.connections.loading = false;
        }
    }

    fn removal_reply(&mut self, result: Result<(), KeyRemovalFailure>) -> Option<ControlToShell> {
        let (epoch, request) = self.connections.remove_request.take()?;
        (self.connections.stage_link == Some(epoch))
            .then_some(ControlToShell::AnthropicKeyRemoved { request, result })
    }

    fn stage_done(
        &mut self,
        epoch: u64,
        generation: u64,
        plan: KeyStageId,
        result: Result<(u64, Secret<AnthropicKey>), KeyStageFailure>,
    ) -> Vec<ControlToShell> {
        let outcome = match result {
            Ok((revision, key)) => {
                if generation == self.connections.generation {
                    self.start_key((Some(revision), Ok(Some(key))));
                }
                Ok(())
            }
            Err(error) => Err(error),
        };
        if self.connections.stage_link != Some(epoch) {
            return Vec::new();
        }
        vec![ControlToShell::AnthropicKeyStageDone {
            plan,
            result: outcome,
        }]
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
        match result {
            Ok(Some(key)) => {
                self.connections.key = Some(key.clone());
                let http = std::sync::Arc::clone(&self.net.http);
                let events = self.reads_tx.clone();
                self.connections.read = Some(tokio::spawn(async move {
                    let result = timeout(Duration::from_secs(15), async {
                        let http = tokio::task::spawn_blocking(move || http())
                            .await
                            .map_err(|_| CheckError::Unconfirmed)?
                            .map_err(|_| CheckError::Unconfirmed)?;
                        models::read(&http, key).await
                    })
                    .await
                    .unwrap_or(Err(CheckError::Unconfirmed));
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

    pub(super) fn connection_event(&mut self, event: ConnectionEvent) -> Vec<ControlToShell> {
        let mut removal = None;
        match event {
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
                    return Vec::new();
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
            ConnectionEvent::Op(op) => {
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
                self.load_key(self.connections.removing.map_or(KeyOp::Load, KeyOp::Remove));
                if !self.connections.loading {
                    removal = self.removal_reply(Err(KeyRemovalFailure::Unconfirmed));
                }
            }
            ConnectionEvent::Loaded { generation, result } => {
                if generation != self.connections.generation || !self.connections.loading {
                    return Vec::new();
                }
                removal = self.key_loaded(generation, result);
            }
            ConnectionEvent::Checked { generation, result } => {
                if generation != self.connections.generation || self.connections.read.is_none() {
                    return Vec::new();
                }
                self.connections.cancel();
                match result {
                    Ok(models) => {
                        self.connections.models = models;
                        self.connections.view.anthropic = KeyView::Ready;
                    }
                    Err(CheckError::Rejected(reason)) => {
                        self.connections.view.anthropic = KeyView::Rejected(reason);
                    }
                    Err(CheckError::Unconfirmed) => {
                        self.connections.view.anthropic = KeyView::Unconfirmed;
                    }
                }
                self.connections.unsent = true;
            }
        }
        std::iter::once(ControlToShell::Connections(self.connections.view))
            .chain(removal)
            .collect()
    }
}

#[cfg(test)]
mod tests;

impl Brain {
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
                self.account
                    .stage_op(self.epoch, op)
                    .iter()
                    .try_for_each(|frame| link.control(frame))
            }
            ConnectionOp::CheckAnthropicKey | ConnectionOp::RemoveAnthropicKey { .. } => {
                self.account_input(Event::Connection(ConnectionEvent::Op(op)), link)
            }
        }
    }
}
