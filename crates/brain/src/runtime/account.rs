//! The account system on the brain's side: it runs the account machine's effects. Credential
//! jobs go to one thread that owns the vault, so every Credential Manager read and write is
//! serialized; Windows can miss a read on one thread while another writes. The listener and
//! the exchange run as tasks, and everything reports back on one channel the core loop polls.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{
    AccountOp, AccountRecovery, AccountView, Action, ConnectionKind, ControlToShell, DataToShell,
    ErrorKey, ModelChoice, ModelInfo, PlanState, ScreenHolds, ScreenOff, SettingsOp, UserError,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::Instant;
use zeroize::Zeroizing;

use crate::config::tuning::{OAuthTuning, TokenTuning};
use crate::connections::account::{
    AccountState, Denied, Done, Effect, Input, Job, Listened, Received, Worker,
};
use crate::connections::catalog::{CatalogError, Listing, default_model, read};
use crate::connections::clock::Clock;
use crate::connections::error::VaultError;
use crate::connections::http_client::{HttpError, SharedHttp};
use crate::connections::oauth::exchange::{ExchangeError, exchange};
use crate::connections::oauth::id_token::{IdentityError, verify, verify_refreshed};
use crate::connections::oauth::refresh::refresh;
use crate::connections::oauth::revoke::revoke;
use crate::connections::oauth::{Authorization, Intent, Launcher, SignIn};
use crate::connections::records::{Account, Revocable};
use crate::connections::vault::Proof;
use crate::context::ConnectionTag;
use crate::coordinator::{Availability, Ready};
use crate::engines::responses::{AccessAsk, Bearer};
use crate::platform::credential::{CredentialStore, CredentialStoreError};

use super::connections::{ConnectionEvent, Connections};
use super::{Brain, Full, Link, Stage, settings};
use crate::coordinator::Input as CoreInput;

pub(super) mod github;
#[cfg(test)]
pub(super) mod github_tests;
mod proof;
#[cfg(test)]
#[path = "account_task_tests.rs"]
mod task_tests;

#[derive(Debug)]
pub(super) enum Work {
    Stage(super::connections::StageWork),
    Account(Job),
    GitHub {
        job: crate::connections::github::lane::Job,
        reply: oneshot::Sender<crate::connections::github::lane::Reply>,
    },
    Key {
        generation: u64,
        op: crate::connections::anthropic::KeyOp,
    },
    /// What `connection`'s sign-in is, for [`proof::Proofs`].
    Proof {
        connection: u64,
        reply: oneshot::Sender<Proof>,
    },
}

/// What a caller of [`Accounts::access`] gets: the bearer, or why there's none.
pub(crate) use crate::engines::responses::Access;

/// Builds the account HTTP client; every exchange builds its own, so a trust store fixed since
/// the last try is read again.
pub(crate) type HttpFactory = Arc<dyn Fn() -> Result<SharedHttp, HttpError> + Send + Sync>;

/// What the account's tasks reach outside the brain through.
#[derive(Clone)]
pub(crate) struct Net {
    pub(crate) http: HttpFactory,
    pub(crate) launcher: Arc<dyn Launcher + Send + Sync>,
    pub(crate) clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Net")
    }
}

/// What the account system hears: its machine's input, or the end of a catalog read.
#[derive(Debug)]
pub(crate) enum Event {
    Verification(github::VerificationEvent),
    GitHub {
        session: Option<u64>,
        reply: crate::connections::github::lane::Reply,
    },
    Connection(ConnectionEvent),
    Machine(Input),
    AudioRepaired,
    /// The read started under `epoch` ended: the models, or what to tell the user, if anything.
    Read {
        epoch: u64,
        result: Result<Listing, Option<UserError>>,
    },
    /// The check of `connection` started under `epoch` answered.
    Proof {
        connection: u64,
        epoch: u64,
        proof: Proof,
    },
}

impl From<Input> for Event {
    fn from(input: Input) -> Self {
        Self::Machine(input)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
enum Discovery {
    #[default]
    Automatic,
    Explicit,
}

/// The account's model catalog, in memory only (design §3 rule 6): one read at a time, cleared
/// when the account stops being usable, so another account never shows this one's models.
#[derive(Debug, Default)]
struct Catalog {
    /// Moves whenever the account stops being usable; a read started before is dropped.
    epoch: u64,
    usable: bool,
    /// The read out, while it waits for the bearer, its client or the answer.
    read: Option<AbortHandle>,
    /// The last successful read, cleared as a unit when the account stops being usable.
    listing: Option<Listing>,
    /// The settled failure a replacement audio pair should hear without retrying.
    failure: Option<UserError>,
    /// The shell hasn't heard the current list.
    unsent: bool,
    /// A local saved-account check recovered; model discovery needs an explicit user action.
    discovery: Discovery,
}

impl Catalog {
    fn viewed(&mut self, view: &AccountView) {
        if !matches!(view, AccountView::SignedIn { .. }) {
            self.discovery = Discovery::Automatic;
        }
        self.set_usable(matches!(
            view,
            AccountView::SignedIn {
                plan: PlanState::Enabled,
                recovery: None | Some(AccountRecovery::Retry),
                ..
            }
        ));
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "one predicate drives both the usability edge and the retained flag"
    )]
    fn set_usable(&mut self, usable: bool) {
        if self.usable && !usable {
            self.epoch += 1;
            self.cancel();
            self.unsent |= self.listing.take().is_some();
            self.discovery = Discovery::Automatic;
        }
        self.usable = usable;
    }

    /// Stops the read out, so it sends nothing more; a refresh its bearer wait started still
    /// runs to its end and is saved.
    fn cancel(&mut self) {
        self.failure = None;
        if let Some(read) = self.read.take() {
            read.abort();
        }
    }
}

#[derive(Debug)]
struct Listening {
    cancelled: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.task.abort();
    }
}

/// The live owner's hold on one repair attempt: dropping it stops the attempt.
#[derive(Debug)]
struct HeldRepair(crate::connections::vault::RepairStop);

impl Drop for HeldRepair {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// The account machine, the credential thread and the tasks it started.
#[derive(Debug)]
pub(crate) struct Accounts {
    pub(super) github: github::GitHub,
    oauth: OAuthTuning,
    state: AccountState,
    pub(super) jobs: Option<std_mpsc::Sender<Work>>,
    /// Fires once the credential thread has dropped the vault and its session lock.
    released: Option<oneshot::Receiver<()>>,
    /// Binds the vault's history link to the store thread, and releases it before that closes.
    pub(crate) history: crate::connections::vault::HistoryBinding,
    events_tx: mpsc::UnboundedSender<Input>,
    events: mpsc::UnboundedReceiver<Input>,
    /// The live attempt; dropping it fences the opener and aborts the listener.
    listener: Option<Listening>,
    pub(super) net: Net,
    /// Callers waiting for the bearer, by ticket.
    tickets: HashMap<u64, oneshot::Sender<Access>>,
    next_ticket: u64,
    catalog: Catalog,
    pub(super) reads_tx: mpsc::UnboundedSender<Event>,
    reads: mpsc::UnboundedReceiver<Event>,
    /// Random per process, so each sign-in's connection is one no earlier sign-in used.
    salt: u64,
    pub(super) connections: Connections,
    /// What the current connection's sign-in may read; it isn't ready until that's known.
    proofs: proof::Proofs,
    /// The repair attempt this pair owns, stopped when the pair ends.
    repair: Option<HeldRepair>,
}

impl Accounts {
    #[cfg(all(test, windows))]
    pub(crate) fn take_release_receipt(&mut self) -> Option<oneshot::Receiver<()>> {
        self.released.take()
    }

    /// Starts the credential thread and waits, blocking, for its startup load, so the vault is
    /// read before anything is served, as the store is; an early Busy is retried for a moment.
    /// Blocking is fine: nothing else runs on the runtime yet.
    ///
    /// # Errors
    /// Fails when native credentials are unsupported, or the thread can't start or load.
    pub(crate) fn spawn<S: CredentialStore + 'static>(
        worker: Worker<S>,
        net: Net,
        oauth: OAuthTuning,
        tokens: TokenTuning,
    ) -> std::io::Result<Self> {
        Self::spawn_with_github(
            worker,
            net,
            github::GitHub::new(tokens.github_refresh_lead),
            oauth,
            tokens.chatgpt_refresh_lead,
        )
    }

    #[cfg(any(test, all(windows, debug_assertions, feature = "test-support")))]
    pub(crate) fn spawn_unconfigured<S: CredentialStore + 'static>(
        worker: Worker<S>,
        net: Net,
    ) -> std::io::Result<Self> {
        let mut accounts = Self::spawn_with_github(
            worker,
            net,
            github::GitHub::with_client(None),
            OAuthTuning::default(),
            TokenTuning::default().chatgpt_refresh_lead,
        )?;
        // Its vault has no history link to wait for.
        accounts
            .proofs
            .release(accounts.jobs.as_ref(), &accounts.reads_tx);
        #[cfg(all(test, not(windows)))]
        accounts.connections.use_offline_route();
        Ok(accounts)
    }

    fn spawn_with_github<S: CredentialStore + 'static>(
        worker: Worker<S>,
        net: Net,
        github: github::GitHub,
        oauth: OAuthTuning,
        chatgpt_refresh_lead: Duration,
    ) -> std::io::Result<Self> {
        let (state, started) = AccountState::start_with_refresh_lead(chatgpt_refresh_lead);
        let (events_tx, events) = mpsc::unbounded_channel();
        let (loaded_tx, loaded) = std_mpsc::channel();
        let (reads_tx, reads) = mpsc::unbounded_channel();
        // Before the credential thread starts, so nothing after it can fail but its load.
        let salt = getrandom::u64().map_err(std::io::Error::other)?;
        let (jobs, released) = spawn(worker, loaded_tx, events_tx.clone(), reads_tx.clone())?;
        let mut accounts = Self {
            github,
            oauth,
            state,
            jobs: Some(jobs),
            released: Some(released),
            history: crate::connections::vault::HistoryBinding::default(),
            events_tx,
            events,
            listener: None,
            net,
            tickets: HashMap::new(),
            next_ticket: 0,
            catalog: Catalog::default(),
            reads_tx,
            reads,
            salt,
            connections: Connections::default(),
            proofs: proof::Proofs::held(),
            repair: None,
        };
        accounts.apply(started);
        let (done, key) = loaded
            .recv()
            .map_err(|_| io::Error::other("the credential thread stopped"))??;
        // No shell is connected yet: only the work it starts matters, not what it would say.
        accounts.handle(done);
        accounts.start_key(key);
        accounts
            .github
            .start(accounts.jobs.as_ref(), &accounts.reads_tx);
        Ok(accounts)
    }

    /// Stops the repair attempt this pair owns, without waiting for the credential thread: a
    /// bind that hasn't claimed it refuses, and one that has settles but never activates.
    pub(crate) fn stop_repair(&mut self) {
        drop(self.repair.take());
    }

    /// Holds `stop` as this pair's repair attempt.
    /// shortcut: only tests hold one; repair ingress must hold it before dispatching work that
    /// can claim the attempt, never for a pair that has ended, and call `stop_repair`
    /// synchronously before queueing its durable cancel.
    #[cfg(test)]
    pub(crate) fn hold_repair(&mut self, stop: crate::connections::vault::RepairStop) {
        self.repair = Some(HeldRepair(stop));
    }

    /// Binds the vault's history link to `writer`, the store thread's, and only then checks what
    /// the current sign-in may read: a replacement's check needs that writer.
    pub(crate) fn bind_history(&mut self, writer: &crate::connections::vault::HistoryLink) {
        self.history.bind(writer);
        self.proofs.release(self.jobs.as_ref(), &self.reads_tx);
    }

    /// The next report from the credential thread or a task.
    pub(crate) async fn next(&mut self) -> Event {
        tokio::select! {
            input = self.events.recv() => {
                Event::Machine(input.expect("the account system holds a sender"))
            }
            Some(read) = self.reads.recv() => read,
            verified = self.github.next_verification() => Event::Verification(verified),
        }
    }

    /// Asks for the bearer, refreshed first when it's due; it comes on the receiver. Dropping the
    /// receiver cancels only the wait: a refresh it started still runs to its end and is saved.
    pub(crate) fn access(&mut self) -> (oneshot::Receiver<Access>, Vec<ControlToShell>) {
        let (answer, receiver) = oneshot::channel();
        (receiver, self.answer_access(answer))
    }

    /// A model round's ask for the bearer: only the connection `wanted` gets it, while it's
    /// the ready one, so a round never sends on an account its run wasn't bound to.
    pub(crate) fn access_for(
        &mut self,
        wanted: Option<ConnectionTag>,
        answer: oneshot::Sender<Access>,
    ) -> Vec<ControlToShell> {
        match self.availability() {
            Availability::Ready(ready) if Some(ready.connection) == wanted => {
                self.answer_access(answer)
            }
            _ => {
                answer.send(Err(Denied::Unusable)).ok();
                Vec::new()
            }
        }
    }

    fn answer_access(&mut self, answer: oneshot::Sender<Access>) -> Vec<ControlToShell> {
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        self.tickets.insert(ticket, answer);
        let now = self.net.clock.unix_seconds();
        self.handle(Input::Access { ticket, now })
    }

    /// Applies an input; what the shell should hear, in order. The catalog is read when the
    /// account becomes usable, and again on Retry or an ordinary connection while no read succeeded.
    pub(crate) fn handle(&mut self, event: impl Into<Event>) -> Vec<ControlToShell> {
        let input = match event.into() {
            Event::Verification(verified) => {
                return self.github.verified(
                    verified,
                    self.jobs.as_ref(),
                    &self.reads_tx,
                    &self.net.http,
                );
            }
            Event::GitHub { session, reply } => {
                return self.github.replied(
                    session,
                    reply,
                    self.jobs.as_ref(),
                    &self.reads_tx,
                    &self.net.http,
                );
            }
            Event::Connection(event) => return self.connection_event(event),
            Event::Machine(input) => input,
            Event::AudioRepaired => {
                self.catalog.unsent |= self.catalog.listing.is_some();
                let mut frames = vec![self.account_frame(self.state.view())];
                if let Some(error) = &self.catalog.failure {
                    frames.push(ControlToShell::Error(error.clone()));
                }
                return frames;
            }
            Event::Read { epoch, result } => return self.read_ended(epoch, result),
            Event::Proof {
                connection,
                epoch,
                proof,
            } => {
                return match self.proofs.found(connection, epoch, proof) {
                    Some(true) => self.start_catalog_read(),
                    Some(false) | None => Vec::new(),
                };
            }
        };
        let retry = matches!(input, Input::Op(AccountOp::Retry));
        let load_models = matches!(input, Input::Op(AccountOp::LoadModels));
        let ask = if self.catalog.discovery == Discovery::Explicit {
            load_models
        } else {
            retry || matches!(input, Input::Connected)
        };
        let local_check = matches!(input, Input::Vault(Done::CheckedSavedSignIn(_)));
        // A new shell hears the list there is.
        self.catalog.unsent |= matches!(input, Input::Connected) && self.catalog.listing.is_some();
        let was_usable = self.catalog.usable;
        let effects = self.state.handle(input);
        self.proofs
            .track(self.state.connection(), self.jobs.as_ref(), &self.reads_tx);
        if local_check && matches!(self.state.view(), AccountView::SignedIn { .. }) {
            self.catalog.discovery = Discovery::Explicit;
        }
        let mut frames = self.apply(effects);
        if !local_check && (ask || !was_usable) {
            frames.extend(self.start_catalog_read());
        }
        frames
    }

    /// Starts the catalog read when the account is usable and has no list or read yet. No bearer
    /// goes out for a sign-in that isn't proved: the read waits for its check, and goes only if
    /// that proves it.
    fn start_catalog_read(&mut self) -> Vec<ControlToShell> {
        let catalog = &self.catalog;
        if !catalog.usable || catalog.read.is_some() || catalog.listing.is_some() {
            return Vec::new();
        }
        if !self.proved() {
            self.proofs.owe_read();
            return Vec::new();
        }
        self.catalog.failure = None;
        let (wait, frames) = self.access();
        let http = Arc::clone(&self.net.http);
        let task = tokio::spawn(read_catalog(wait, http, self.reads_tx.clone()));
        self.catalog.read = Some(task.abort_handle());
        drop(tokio::spawn(catalog_task(
            self.catalog.epoch,
            task,
            self.reads_tx.clone(),
        )));
        frames
    }

    /// What the current sign-in may read, once its check answered.
    fn proof(&self) -> Option<Proof> {
        self.proofs.proof(self.state.connection()?)
    }

    /// The current sign-in is proved ordinary, or recovered under its own connection.
    fn proved(&self) -> bool {
        matches!(self.proof(), Some(Proof::Ordinary | Proof::Recovered(_)))
    }

    /// The cutoff the current sign-in's reads keep to, when it's a recovered one.
    pub(super) fn recovered(&self) -> Option<crate::store::Boundary> {
        match self.proof()? {
            Proof::Recovered(cut) => Some(cut),
            Proof::Ordinary | Proof::Unproved => None,
        }
    }

    /// A sign-in is held whose history a turn mustn't read unscoped: one not proved yet or
    /// unprovable, or a recovered one that can't publish its cutoff because it isn't ready.
    pub(super) fn reads_fenced(&self) -> bool {
        if self.state.connection().is_none() {
            return false;
        }
        match self.proof() {
            Some(Proof::Ordinary) => false,
            Some(Proof::Recovered(_)) => !matches!(self.availability(), Availability::Ready(_)),
            Some(Proof::Unproved) | None => true,
        }
    }

    /// A catalog read ended; one for an account that has since changed is dropped.
    fn read_ended(
        &mut self,
        epoch: u64,
        result: Result<Listing, Option<UserError>>,
    ) -> Vec<ControlToShell> {
        if epoch != self.catalog.epoch {
            return Vec::new();
        }
        self.catalog.read = None;
        match result {
            Ok(listing) => {
                self.catalog.listing = Some(listing);
                self.catalog.unsent = true;
                if std::mem::take(&mut self.catalog.discovery) == Discovery::Explicit {
                    vec![self.account_frame(self.state.view())]
                } else {
                    Vec::new()
                }
            }
            Err(None) => Vec::new(),
            Err(Some(error)) => {
                let unavailable = error.key() == ErrorKey::PlanUnavailable;
                self.catalog.failure = Some(error.clone());
                let mut frames = vec![ControlToShell::Error(error)];
                if unavailable {
                    frames.extend(self.handle(Input::PlanUnavailable));
                }
                frames
            }
        }
    }

    /// Whether runs can be answered: a usable account's connection and catalog, or what the
    /// user can do. An accepted sign-out is already unavailable.
    pub(super) fn availability(&self) -> Availability {
        if self.catalog.discovery == Discovery::Explicit {
            return Availability::Unavailable(ErrorKey::CredentialsUnavailable.into());
        }
        let error = |key, action| {
            UserError::new(key, Vec::new(), vec![action]).expect("one action is within the limit")
        };
        let reason = match self.state.view() {
            AccountView::SignedIn {
                plan: PlanState::Enabled,
                recovery: None | Some(AccountRecovery::Retry | AccountRecovery::LoadModels),
                ..
            } => match self
                .state
                .connection()
                .map(|connection| (connection, self.proof()))
            {
                Some((_, None)) => return Availability::Finding,
                Some((connection, Some(Proof::Ordinary | Proof::Recovered(_)))) => {
                    return Availability::Ready(Ready {
                        connection: ConnectionTag(connection),
                        models: self.models().unwrap_or_default().to_vec(),
                        #[cfg(any(test, debug_assertions))]
                        trusted_draw: false,
                        images: self
                            .catalog
                            .listing
                            .as_ref()
                            .map(|listing| listing.images.clone())
                            .unwrap_or_default(),
                    });
                }
                Some((_, Some(Proof::Unproved))) | None => {
                    error(ErrorKey::SignInNeeded, Action::SignIn)
                }
            },
            AccountView::SignedIn {
                recovery: Some(AccountRecovery::SignInAgain),
                ..
            } => error(
                self.state.ended_by().unwrap_or(ErrorKey::SessionEnded),
                Action::SignIn,
            ),
            AccountView::SignedIn {
                plan: PlanState::Declined,
                ..
            } => error(ErrorKey::PlanNotEnabled, Action::Enable),
            AccountView::SignedIn {
                plan: PlanState::Unavailable,
                ..
            } => UserError::from(ErrorKey::PlanUnavailable),
            AccountView::Busy => error(ErrorKey::CredentialsUnavailable, Action::Retry),
            AccountView::SavedSignInUnavailable { .. } => {
                UserError::from(ErrorKey::CredentialsUnavailable)
            }
            AccountView::SignedOut { .. }
            | AccountView::SigningIn
            | AccountView::SigningOut { .. } => error(ErrorKey::SignInNeeded, Action::SignIn),
        };
        Availability::Unavailable(reason)
    }

    /// Where the account stands now.
    pub(crate) fn view(&self) -> AccountView {
        self.state.view()
    }

    fn account_frame(&self, mut view: AccountView) -> ControlToShell {
        if self.catalog.discovery == Discovery::Explicit
            && let AccountView::SignedIn {
                plan: PlanState::Enabled,
                recovery: recovery @ None,
                ..
            } = &mut view
        {
            *recovery = Some(AccountRecovery::LoadModels);
        }
        ControlToShell::Account(view)
    }

    /// The account's catalog epoch while it can send model requests: its generation for jobs.
    pub(super) fn sendable(&self) -> Option<u64> {
        (self.catalog.usable && self.catalog.discovery == Discovery::Automatic)
            .then_some(self.catalog.epoch)
    }

    /// The last catalog read in this epoch, if one succeeded.
    pub(super) fn models(&self) -> Option<&[ModelInfo]> {
        self.catalog
            .listing
            .as_ref()
            .map(|listing| listing.models.as_slice())
    }

    #[cfg(test)]
    pub(super) fn catalog_read_pending(&self) -> bool {
        self.catalog.read.is_some()
    }

    #[cfg(test)]
    pub(super) fn vault_load_pending(&self) -> bool {
        self.state.vault_load_pending()
    }

    /// A read catalog of `models`, of which `images` take image input.
    #[cfg(test)]
    pub(super) fn read_catalog(
        &mut self,
        models: Vec<ModelInfo>,
        images: Vec<kumochi_protocol::message::ModelSlug>,
    ) {
        self.catalog.listing = Some(Listing::fixture(models, images));
    }

    /// Whether `choice` can be sent an image: the catalog's own model on the account's connection,
    /// listed as taking image input. Nothing else does, so the screen is skipped before capture.
    pub(super) fn takes_images(&self, choice: &ModelChoice) -> bool {
        choice.connection == ConnectionKind::ChatGpt
            && self
                .catalog
                .listing
                .as_ref()
                .is_some_and(|listing| listing.images.contains(&choice.model))
    }

    /// The image gate: whether a hold on `choice` also shows the model the window under the
    /// pointer, given whether this platform has window capture and whether the user allows it.
    /// With no catalog read, or no model chosen, it isn't known yet, so it's `NotReady` rather
    /// than `ModelCantSee`; the switch comes first, so turning it off always reads as off.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the caller supplies the platform's capture and the user's switch"
    )]
    pub(super) fn screen_holds(
        &self,
        choice: Option<&ModelChoice>,
        capture: bool,
        enabled: bool,
    ) -> ScreenHolds {
        if !capture {
            return ScreenHolds::Off(ScreenOff::Unsupported);
        }
        if !enabled {
            return ScreenHolds::Off(ScreenOff::TurnedOff);
        }
        let (Some(models), Some(choice)) = (self.models(), choice) else {
            return ScreenHolds::Off(ScreenOff::NotReady);
        };
        models
            .iter()
            .find(|info| info.slug == choice.model)
            .filter(|_| self.takes_images(choice))
            .map_or(ScreenHolds::Off(ScreenOff::ModelCantSee), |info| {
                ScreenHolds::On {
                    model: info.display_name.clone(),
                    connection: choice.connection,
                    slug: choice.model.clone(),
                    revision: kumochi_protocol::message::NoticeRevision::FIRST,
                }
            })
    }

    /// The catalog for the shell, once each time it changes or a shell connects.
    pub(super) fn models_unsent(&mut self) -> Option<Vec<ModelInfo>> {
        std::mem::take(&mut self.catalog.unsent).then(|| self.models().unwrap_or_default().to_vec())
    }

    fn apply(&mut self, effects: Vec<Effect>) -> Vec<ControlToShell> {
        let mut frames = Vec::new();
        for effect in effects {
            match effect {
                Effect::View(view) => {
                    self.catalog.viewed(&view);
                    frames.push(self.account_frame(view));
                }
                Effect::Error(error) => frames.push(ControlToShell::Error(error)),
                Effect::Vault(job) => {
                    let sent = self
                        .jobs
                        .as_ref()
                        .is_some_and(|jobs| jobs.send(Work::Account(job)).is_ok());
                    if !sent {
                        tracing::error!("the credential thread has stopped");
                    }
                }
                Effect::Listen {
                    attempt,
                    host,
                    saved,
                    intent,
                } => {
                    drop(self.listener.take());
                    let cancelled = Arc::new(AtomicBool::new(false));
                    let task = tokio::spawn(listen(
                        attempt,
                        host,
                        saved,
                        intent,
                        (self.oauth.sign_in, Arc::clone(&cancelled)),
                        Arc::clone(&self.net.launcher),
                        self.events_tx.clone(),
                    ));
                    self.listener = Some(Listening { cancelled, task });
                }
                Effect::Close => drop(self.listener.take()),
                // Never aborted: a sent token request may already have issued tokens.
                Effect::Exchange {
                    attempt,
                    authorization,
                } => drop(tokio::spawn(exchange_task(
                    attempt,
                    // The top bit keeps it apart from sets saved before connections, which read 0.
                    self.salt.wrapping_add(attempt) | 1 << 63,
                    authorization,
                    self.net.clone(),
                    self.events_tx.clone(),
                ))),
                Effect::Revoke(entries) => drop(tokio::spawn(revoke_task(
                    entries,
                    Instant::now() + self.oauth.revoke,
                    self.oauth.backoff,
                    Arc::clone(&self.net.http),
                    self.events_tx.clone(),
                ))),
                // Never aborted: the old token may already have rotated.
                Effect::Refresh {
                    generation,
                    client,
                    refresh_token,
                } => drop(tokio::spawn(refresh_task(
                    generation,
                    client,
                    refresh_token,
                    Instant::now() + self.oauth.refresh,
                    self.net.clone(),
                    self.events_tx.clone(),
                ))),
                Effect::Validate {
                    generation,
                    client,
                    subject,
                    id_token,
                } => drop(tokio::spawn(validate_task(
                    generation,
                    [client, subject, id_token],
                    self.net.clone(),
                    self.events_tx.clone(),
                ))),
                Effect::Access {
                    ticket,
                    generation,
                    result,
                } => {
                    // A caller that stopped waiting just doesn't hear it.
                    if let Some(answer) = self.tickets.remove(&ticket) {
                        answer
                            .send(result.map(|token| Bearer { generation, token }))
                            .ok();
                    }
                }
            }
        }
        frames
    }

    /// Stops taking work and cancels the catalog read. A refresh already out runs to its end, and its result's save, until
    /// `refresh_by`, so a rotated set the provider issued isn't lost; past that its token stays
    /// marked as sent, and the next start never sends it again. Then it waits for the credential
    /// job already dispatched, past `deadline` if it must, so it never overlaps the instance
    /// capability's delete; then the vault and its session lock are gone. Jobs not yet
    /// dispatched are dropped: a pending flag they would have cleared stays set, and the next
    /// start counts it as uncertainty. Reports the worst of the refresh against `refresh_by`,
    /// the credential job's release against `deadline`, and any issued set closing left unsaved.
    pub(crate) async fn close(&mut self, deadline: Instant, refresh_by: Instant) -> Stage {
        self.stop_repair();
        drop(self.listener.take());
        self.proofs.close();
        self.github
            .stop(self.jobs.as_ref(), &self.reads_tx, &self.net.http);
        self.catalog.cancel();
        self.connections.close();
        let settled = tokio::time::timeout_at(refresh_by, async {
            while self.state.refresh_out() || self.github.rotating() {
                let input = self.next().await;
                // No shell hears it: the brain is closing.
                self.handle(input);
            }
        })
        .await;
        let refreshed = if settled.is_ok() {
            Stage::seen(Instant::now(), refresh_by)
        } else {
            tracing::warn!("a refresh is still out at shutdown; its token stays marked as sent");
            self.github.stop_rotation();
            Stage::Outstanding
        };
        // One the settling started.
        self.catalog.cancel();
        while self.github.pending() {
            let input = self.next().await;
            self.handle(input);
        }
        self.jobs = None;
        // No receipt left means no release to confirm.
        let released = if let Some(mut released) = self.released.take() {
            if let Ok(receipt) = tokio::time::timeout_at(deadline, &mut released).await {
                receipt.map_or(Stage::Failed, |()| Stage::seen(Instant::now(), deadline))
            } else {
                // It can't be aborted, and the instance capability is deleted next, so it ends
                // first.
                tracing::warn!("a credential job is still running at the deadline; waiting for it");
                released.await.map_or(Stage::Failed, |()| Stage::Late)
            }
        } else {
            Stage::Failed
        };
        while self.github.verifying() {
            let verified = self.github.next_verification().await;
            self.handle(Event::Verification(verified));
        }
        // A refused save holds its set, a GitHub save or rotation started after the job closed
        // can't finish, and one that ended in an error can't either: an issued set was never saved.
        let unsaved = if self.state.held()
            || self.github.pending()
            || self.github.rotating()
            || self.github.rotation_failed()
        {
            Stage::Failed
        } else {
            Stage::Confirmed
        };
        refreshed.max(released).max(unsaved)
    }
}

impl Brain {
    /// A round asks for the bearer, of the connection the coordinator says its send may use, or
    /// says the provider refused the one it had.
    pub(super) fn access_asked(
        &mut self,
        ask: AccessAsk,
        link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        let (key, answer) = match ask {
            AccessAsk::Bearer { key, answer } => (key, answer),
            AccessAsk::Refused(generation) => {
                return self.account_input(Input::Refused { generation }, link);
            }
        };
        let wanted = self.coordinator.sending(key);
        let frames = self.account.access_for(wanted, answer);
        let Some(link) = link else {
            return Ok(());
        };
        frames.iter().try_for_each(|frame| link.control(frame))
    }

    /// The brain's own repair of `default_model`, for one connection's half: Claude Code's only
    /// while `ChatGPT` has no account at all, else `ChatGPT`'s, so a session that ends for a while
    /// never moves anyone to Claude Code. An invalid default is never touched.
    pub(super) fn repaired_default(&self) -> Option<kumochi_protocol::message::ModelChoice> {
        if matches!(
            self.config.default_choice(),
            crate::store::choice::StoredChoice::Invalid
        ) {
            return None;
        }
        let current = &self.config.view().default_model;
        if matches!(self.account.view(), AccountView::SignedOut { .. }) {
            return self.claude.repaired_default(current.as_ref());
        }
        self.account
            .catalog
            .listing
            .as_ref()
            .and_then(|listing| default_model(current.as_ref(), listing))
    }

    /// Queues the brain's own change to `default_model` when [`repaired_default`] calls for one.
    ///
    /// [`repaired_default`]: Self::repaired_default
    pub(super) fn repair_default(&mut self, link: Option<&mut Link<'_, '_>>) -> Result<(), Full> {
        let Some(choice) = self.repaired_default() else {
            return Ok(());
        };
        self.settings_ops.push_back(settings::Queued {
            change: settings::Change::User(SettingsOp::SetDefaultModel(choice)),
            epoch: settings::BRAIN,
        });
        self.pump_settings(link)
    }

    /// Applies an account input and tells the connected shell, if any, what changed. A catalog
    /// read that leaves `default_model` unset or unlisted queues the brain's own change to it.
    pub(super) fn account_input(
        &mut self,
        input: impl Into<Event>,
        mut link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        let event = input.into();
        // A stale check's answer reaches neither the account nor anything after it here.
        if let Event::Proof {
            connection, epoch, ..
        } = event
            && !self.account.proofs.awaits(connection, epoch)
        {
            return Ok(());
        }
        let read = matches!(event, Event::Read { .. });
        if matches!(
            event,
            Event::Machine(
                Input::Op(AccountOp::CheckSavedSignIn) | Input::Vault(Done::CheckedSavedSignIn(_))
            )
        ) {
            let frames = self.account.handle(event);
            // A saved sign-in it reads back is held before its check answers.
            self.fence_chatgpt_reads();
            return link.map_or(Ok(()), |link| {
                frames
                    .iter()
                    .try_for_each(|frame| link.control(&self.composed(frame)))
            });
        }
        let sendable = self.account.sendable();
        let frames = self.account.handle(event);
        // The coordinator hears it before anything else, so a job's write is fenced at once.
        let now = self.account.sendable();
        if now != sendable {
            let out = sendable.map(|generation| CoreInput::SignedOut { generation });
            let signed_in = now.map(|generation| CoreInput::SignedIn { generation });
            for input in out.into_iter().chain(signed_in) {
                if let Some(effects) = self.handle(input) {
                    self.apply(effects, link.as_deref_mut()).map_err(|_| Full)?;
                }
            }
        }
        self.availability_changed(link.as_deref_mut())
            .map_err(|_| Full)?;
        // The run sending on a refused key hears why before its route change ends it as unavailable.
        if let Some(effects) = self
            .account
            .connections
            .take_refused()
            .and_then(|refused| self.handle(CoreInput::KeyRefused(refused)))
        {
            self.apply(effects, link.as_deref_mut()).map_err(|_| Full)?;
        }
        self.key_route_changed(link.as_deref_mut())?;
        if read {
            self.repair_default(link.as_deref_mut())?;
        }
        let Some(link) = link else {
            return Ok(());
        };
        frames
            .iter()
            .try_for_each(|frame| link.control(&self.composed(frame)))?;
        if let Some(models) = self.account.connections.models_unsent() {
            link.data(&models)?;
        }
        match self.account.models_unsent() {
            Some(models) => link.data(&DataToShell::Models {
                connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
                models,
            }),
            None => Ok(()),
        }
    }
}

/// The credential thread: runs each job whole, in order, and reports it; the first, the startup
/// load, to `loaded`.
fn spawn<S: CredentialStore + 'static>(
    worker: Worker<S>,
    loaded: std_mpsc::Sender<io::Result<(Input, crate::connections::anthropic::KeyResult)>>,
    events: mpsc::UnboundedSender<Input>,
    reads: mpsc::UnboundedSender<Event>,
) -> std::io::Result<(std_mpsc::Sender<Work>, oneshot::Receiver<()>)> {
    let (jobs, queued) = std_mpsc::channel::<Work>();
    let (released_tx, released) = oneshot::channel();
    thread::Builder::new()
        .name("credentials".to_owned())
        .spawn(move || {
            let mut loaded = Some(loaded);
            // After `loaded`, so an unwind drops the vault and its lock before the load hears.
            let mut worker = worker;
            let mut refused = None;
            // shortcut: a native credential call that never returns holds every later job and
            // shutdown's release; job order and the runtime generation guard keep removal safe,
            // and only a worker that can abandon a call would bound the wait.
            for job in queued {
                let job = match job {
                    Work::Stage(work) => {
                        if reads
                            .send(Event::Connection(work.run(&mut worker)))
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                    Work::Account(job) => job,
                    Work::GitHub { job, reply } => {
                        reply.send(worker.github(job)).ok();
                        continue;
                    }
                    Work::Proof { connection, reply } => {
                        reply.send(worker.proof(connection)).ok();
                        continue;
                    }
                    Work::Key { generation, op } => {
                        if reads
                            .send(Event::Connection(ConnectionEvent::Loaded {
                                generation,
                                result: worker.anthropic(op),
                            }))
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                };
                let sent = match (loaded.take(), worker.run(job)) {
                    (
                        Some(loaded),
                        Done::Loaded(Err(VaultError::Store(
                            error @ CredentialStoreError::Unsupported,
                        ))),
                    ) => {
                        refused = Some((loaded, io::Error::new(io::ErrorKind::Unsupported, error)));
                        break;
                    }
                    (Some(loaded), done) => loaded
                        .send(Ok((
                            Input::Vault(done),
                            worker.anthropic(crate::connections::anthropic::KeyOp::Load),
                        )))
                        .is_ok(),
                    (None, done) => events.send(Input::Vault(done)).is_ok(),
                };
                if !sent {
                    break;
                }
            }
            drop(worker);
            // Nobody waiting on the release is fine.
            released_tx.send(()).ok();
            // Only now, so a start refused here has already seen the vault and its lock go.
            if let Some((loaded, error)) = refused {
                loaded.send(Err(error)).ok();
            }
        })?;
    Ok((jobs, released))
}

async fn listen(
    attempt: u64,
    host: Secret<Zeroizing<String>>,
    saved: Option<Secret<Account>>,
    intent: Intent,
    (limit, cancelled): (Duration, Arc<AtomicBool>),
    launcher: Arc<dyn Launcher + Send + Sync>,
    events: mpsc::UnboundedSender<Input>,
) {
    let result = async {
        let sign_in = SignIn::bind(
            host.expose(),
            saved.as_ref().map(Secret::expose),
            intent,
            limit,
        )
        .await
        .map_err(Listened::Bind)?;
        sign_in
            .open_browser(launcher, &cancelled)
            .await
            .map_err(Listened::Launch)?;
        sign_in
            .receive()
            .await
            .map(Listened::Authorized)
            .map_err(Listened::Receive)
    }
    .await;
    let result = result.unwrap_or_else(|failed| failed);
    // The account system outlives its tasks, so a closed channel means the brain is exiting.
    events.send(Input::Listened { attempt, result }).ok();
}

async fn account_http(http: HttpFactory, deadline: Instant) -> Result<SharedHttp, ExchangeError> {
    if Instant::now() >= deadline {
        return Err(ExchangeError::PreDispatchTimeout);
    }
    let built = tokio::time::timeout_at(deadline, tokio::task::spawn_blocking(move || http()))
        .await
        .map_err(|_| ExchangeError::PreDispatchTimeout)?;
    if Instant::now() >= deadline {
        return Err(ExchangeError::PreDispatchTimeout);
    }
    built
        .map_err(|_| HttpError::Configuration)?
        .map_err(ExchangeError::Http)
}

async fn revoke_task(
    entries: Vec<Secret<Revocable>>,
    deadline: Instant,
    backoff: [Duration; 2],
    http: HttpFactory,
    events: mpsc::UnboundedSender<Input>,
) {
    let ended = match account_http(http, deadline).await {
        Ok(http) => Input::Revoked(revoke(&http, entries, deadline, backoff).await),
        Err(_) => Input::Revoked(Vec::new()),
    };
    events.send(ended).ok();
}

/// Reads the catalog with the bearer `wait` brings, telling the account on `reads` when the
/// provider refused it. Aborting it drops only that wait, never a refresh it started.
async fn read_catalog(
    wait: oneshot::Receiver<Access>,
    http: HttpFactory,
    reads: mpsc::UnboundedSender<Event>,
) -> Result<Listing, Option<UserError>> {
    let bearer = match wait.await {
        Ok(Ok(bearer)) => bearer,
        Ok(Err(Denied::Failed(error))) => return Err(Some(error)),
        Ok(Err(Denied::Unusable)) | Err(_) => return Err(None),
    };
    let failed = |error: HttpError| Some(CatalogError::Http(error).failure());
    let http = tokio::task::spawn_blocking(move || http())
        .await
        .map_err(|_| failed(HttpError::Configuration))?
        .map_err(failed)?;
    read(&http, bearer.token).await.map_err(|error| {
        if matches!(error, CatalogError::Refused { status: 401, .. }) {
            let generation = bearer.generation;
            reads.send(Input::Refused { generation }.into()).ok();
        }
        Some(error.failure())
    })
}

/// Reports the end of `task`, a read in its own task so one that panics still ends; a cancelled
/// one reports nothing.
async fn catalog_task(
    epoch: u64,
    task: JoinHandle<Result<Listing, Option<UserError>>>,
    reads: mpsc::UnboundedSender<Event>,
) {
    let result = match task.await {
        Ok(result) => result,
        Err(error) if error.is_cancelled() => return,
        Err(_) => {
            // The payload isn't logged.
            tracing::error!("a catalog read stopped before it reported");
            Err(Some(CatalogError::Http(HttpError::Transport).failure()))
        }
    };
    reads.send(Event::Read { epoch, result }).ok();
}

async fn refresh_task(
    generation: u64,
    client: Secret<Zeroizing<String>>,
    refresh_token: Secret<Zeroizing<String>>,
    deadline: Instant,
    net: Net,
    events: mpsc::UnboundedSender<Input>,
) {
    let build = Arc::clone(&net.http);
    // Its own task, so one that panics still ends the flight; the payload isn't logged.
    let result = tokio::spawn(async move {
        let http = account_http(build, deadline).await?;
        refresh(&http, client.expose(), refresh_token.expose(), deadline).await
    })
    .await
    .unwrap_or_else(|_| {
        tracing::error!("a refresh task stopped before it reported");
        // The request may have gone out, so its outcome is unknown.
        Err(ExchangeError::Http(HttpError::Transport))
    });
    // Its lifetime starts on arrival.
    let now = net.clock.unix_seconds();
    events
        .send(Input::Refreshed {
            generation,
            result,
            now,
        })
        .ok();
}

async fn validate_task(
    generation: u64,
    [client, subject, id_token]: [Secret<Zeroizing<String>>; 3],
    net: Net,
    events: mpsc::UnboundedSender<Input>,
) {
    let (build, clock) = (Arc::clone(&net.http), Arc::clone(&net.clock));
    // Its own task, so one that panics still ends the flight; the payload isn't logged.
    let result = tokio::spawn(async move {
        let http = tokio::task::spawn_blocking(move || build())
            .await
            .map_err(|_| IdentityError::Http(HttpError::Configuration))??;
        verify_refreshed(
            &http,
            &id_token,
            client.expose(),
            subject.expose(),
            clock.as_ref(),
        )
        .await
    })
    .await
    .unwrap_or_else(|_| {
        tracing::error!("an identity check task stopped before it reported");
        // The set stays pending, for the next use to check again.
        Err(IdentityError::Discovery)
    });
    let now = net.clock.unix_seconds();
    events
        .send(Input::Validated {
            generation,
            result,
            now,
        })
        .ok();
}

pub(super) async fn exchange_task(
    attempt: u64,
    connection: u64,
    authorization: Authorization,
    net: Net,
    events: mpsc::UnboundedSender<Input>,
) {
    let result = async {
        // Building reads the system trust store, which blocks.
        let build = Arc::clone(&net.http);
        let http = tokio::task::spawn_blocking(move || build())
            .await
            .map_err(|_| ExchangeError::Http(HttpError::Configuration))?
            .map_err(ExchangeError::Http)?;
        let tokens = exchange(&http, &authorization).await?;
        // Its lifetime starts on arrival, not after the identity check.
        let now = net.clock.unix_seconds();
        let id_token = tokens
            .id_token
            .as_ref()
            .expect("an exchange's set carries an identity token");
        let identity = verify(
            &http,
            id_token,
            authorization.client_id.expose(),
            authorization.nonce.expose(),
            net.clock.as_ref(),
        )
        .await;
        Ok::<_, ExchangeError>(Received {
            tokens,
            identity,
            now,
            connection,
        })
    }
    .await;
    events.send(Input::Exchanged { attempt, result }).ok();
}
