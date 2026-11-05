//! Anthropic connection work shares the account worker without entering its token state machine.

use std::fmt;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{
    ClaudeCodeView, ConnectionKind, ConnectionOp, ConnectionsView, ControlToShell, DataToShell,
    KeyView,
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
    Loaded {
        generation: u64,
        result: KeyResult,
    },
    Checked {
        generation: u64,
        result: Result<Vec<CatalogModel>, CheckError>,
    },
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

    pub(super) fn connection_event(&mut self, event: ConnectionEvent) -> Vec<ControlToShell> {
        match event {
            ConnectionEvent::Op(op) => match op {
                ConnectionOp::AnthropicKeyStaged => self.load_key(KeyOp::Adopt),
                ConnectionOp::CheckAnthropicKey => self.load_key(KeyOp::Load),
                ConnectionOp::RemoveAnthropicKey => self.load_key(KeyOp::Remove),
                _ => return Vec::new(),
            },
            ConnectionEvent::Loaded { generation, result } => {
                if generation != self.connections.generation || !self.connections.loading {
                    return Vec::new();
                }
                self.connections.loading = false;
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
            ConnectionOp::AnthropicKeyStaged
            | ConnectionOp::CheckAnthropicKey
            | ConnectionOp::RemoveAnthropicKey => {
                self.account_input(Event::Connection(ConnectionEvent::Op(op)), link)
            }
        }
    }
}
