//! Anthropic connection work shares the account worker without entering its token state machine.

use std::fmt;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{
    ClaudeCodeView, ConnectionKind, ConnectionOp, ConnectionsView, ControlToShell, DataToShell,
    KeyStageFailure, KeyStageId, KeyStagePlan, KeyView,
};
use tokio::task::JoinHandle;
use tokio::time::{Duration, timeout};

use crate::connections::anthropic::{
    KeyError, KeyOp,
    key::AnthropicKey,
    models::{self, CatalogModel, CheckError},
};

use super::account::{Accounts, Event, Work};
use super::{Brain, Full, Link};

pub(super) type KeyResult = Result<Option<Secret<AnthropicKey>>, KeyError>;

pub(crate) enum ConnectionEvent {
    Op(ConnectionOp),
    StagePlan {
        epoch: u64,
        request: u64,
        result: Result<KeyStagePlan, KeyStageFailure>,
    },
    StageDone {
        epoch: u64,
        generation: u64,
        plan: KeyStageId,
        result: Result<Secret<AnthropicKey>, KeyStageFailure>,
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

#[derive(Debug)]
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

fn stage_error(error: &KeyError) -> KeyStageFailure {
    match error {
        KeyError::Rejected(reason) => KeyStageFailure::Rejected(*reason),
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
                    .map(|plan| KeyStagePlan {
                        id: plan.generation.stage_id(),
                        len: u16::try_from(plan.bytes).expect("a paste length fits u16"),
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
    removing: bool,
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
                claude_code: ClaudeCodeView::NotFound,
            },
            models: Vec::new(),
            unsent: false,
            read: None,
            loading: false,
            removing: false,
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
                self.connections.stage_admission = None;
                self.connections.stage_request = Some(request);
                if self.connections.removing || self.connections.stage_link != Some(epoch) {
                    return vec![ControlToShell::AnthropicKeyStage {
                        request,
                        result: Err(KeyStageFailure::Unavailable),
                    }];
                }
                StageWork::Begin {
                    epoch,
                    request,
                    len,
                }
            }
            ConnectionOp::AnthropicKeyStaged { plan } => {
                if self.connections.removing
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
        if matches!(result, Ok(None)) {
            return;
        }
        let generation = self.connections.advance();
        self.connections.loading = true;
        self.connections.view.anthropic = KeyView::Checking;
        self.reads_tx
            .send(Event::Connection(ConnectionEvent::Loaded {
                generation,
                result,
            }))
            .ok();
    }

    pub(super) fn load_key(&mut self, op: KeyOp) {
        if matches!(op, KeyOp::Remove) {
            self.connections.stage_request = None;
            self.connections.stage_admission = None;
        }
        let generation = self.connections.advance();
        self.connections.view.anthropic = KeyView::Checking;
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

    fn stage_done(
        &mut self,
        epoch: u64,
        generation: u64,
        plan: KeyStageId,
        result: Result<Secret<AnthropicKey>, KeyStageFailure>,
    ) -> Vec<ControlToShell> {
        let outcome = match result {
            Ok(key) => {
                if generation == self.connections.generation {
                    self.start_key(Ok(Some(key)));
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

    pub(super) fn connection_event(&mut self, event: ConnectionEvent) -> Vec<ControlToShell> {
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
                let op = match op {
                    ConnectionOp::CheckAnthropicKey => KeyOp::Load,
                    ConnectionOp::RemoveAnthropicKey => KeyOp::Remove,
                    _ => return Vec::new(),
                };
                self.connections.removing |= matches!(op, KeyOp::Remove);
                self.load_key(if self.connections.removing {
                    KeyOp::Remove
                } else {
                    op
                });
            }
            ConnectionEvent::Loaded { generation, result } => {
                if generation != self.connections.generation || !self.connections.loading {
                    return Vec::new();
                }
                self.connections.loading = false;
                self.connections.removing &= !matches!(result, Ok(None));
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
                    Err(KeyError::Vault(_)) => {
                        self.connections.view.anthropic = KeyView::Unconfirmed;
                    }
                }
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
        vec![ControlToShell::Connections(self.connections.view)]
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
            ConnectionOp::CheckAnthropicKey | ConnectionOp::RemoveAnthropicKey => {
                self.account_input(Event::Connection(ConnectionEvent::Op(op)), link)
            }
        }
    }
}
