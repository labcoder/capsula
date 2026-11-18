//! The account system on the brain's side: it runs the account machine's effects. Credential
//! jobs go to one thread that owns the vault, so every Credential Manager read and write is
//! serialized; Windows can miss a read on one thread while another writes. The listener and
//! the exchange run as tasks, and everything reports back on one channel the core loop polls.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::thread;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{
    AccountOp, AccountRecovery, AccountView, Action, ConnectionKind, ControlToShell, DataToShell,
    ErrorKey, ModelChoice, ModelInfo, PlanState, ScreenHolds, ScreenOff, SettingsOp, UserError,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::Instant;
use zeroize::Zeroizing;

use crate::connections::account::{
    AccountState, Denied, Effect, Input, Job, Listened, Received, Worker,
};
use crate::connections::catalog::{CatalogError, Listing, default_model, read};
use crate::connections::clock::Clock;
use crate::connections::http_client::{HttpError, SharedHttp};
use crate::connections::oauth::exchange::{ExchangeError, exchange};
use crate::connections::oauth::id_token::{IdentityError, verify, verify_refreshed};
use crate::connections::oauth::refresh::refresh;
use crate::connections::oauth::revoke::{BUDGET, revoke};
use crate::connections::oauth::{Authorization, Intent, LaunchError, Launcher, SignIn};
use crate::connections::records::{Account, Revocable};
use crate::context::ConnectionTag;
use crate::coordinator::{Availability, Ready};
use crate::engines::responses::{AccessAsk, Bearer};
use crate::platform::credential::CredentialStore;

use super::connections::{ConnectionEvent, Connections};
use super::{Brain, Full, Link, settings};
use crate::coordinator::Input as CoreInput;

pub(super) mod github;
#[cfg(test)]
pub(super) mod github_tests;

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
    /// The read started under `epoch` ended: the models, or what to tell the user, if anything.
    Read {
        epoch: u64,
        result: Result<Listing, Option<UserError>>,
    },
}

impl From<Input> for Event {
    fn from(input: Input) -> Self {
        Self::Machine(input)
    }
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
    /// The shell hasn't heard the current list.
    unsent: bool,
}

impl Catalog {
    fn viewed(&mut self, view: &AccountView) {
        let usable = matches!(
            view,
            AccountView::SignedIn {
                plan: PlanState::Enabled,
                recovery: None | Some(AccountRecovery::Retry),
                ..
            }
        );
        if self.usable && !usable {
            self.epoch += 1;
            self.cancel();
            self.unsent |= self.listing.take().is_some();
        }
        self.usable = usable;
    }

    /// Stops the read out, so it sends nothing more; a refresh its bearer wait started still
    /// runs to its end and is saved.
    fn cancel(&mut self) {
        if let Some(read) = self.read.take() {
            read.abort();
        }
    }
}

/// The account machine, the credential thread and the tasks it started.
#[derive(Debug)]
pub(crate) struct Accounts {
    pub(super) github: github::GitHub,
    state: AccountState,
    pub(super) jobs: Option<std_mpsc::Sender<Work>>,
    /// Fires once the credential thread has dropped the vault and its session lock.
    released: Option<oneshot::Receiver<()>>,
    events_tx: mpsc::UnboundedSender<Input>,
    events: mpsc::UnboundedReceiver<Input>,
    /// The live attempt's listener; aborting it closes the loopback.
    listener: Option<JoinHandle<()>>,
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
}

impl Accounts {
    /// Starts the credential thread and waits, blocking, for its startup load, so the vault is
    /// read before anything is served, as the store is; an early Busy is retried for a moment.
    /// Blocking is fine: nothing else runs on the runtime yet.
    ///
    /// # Errors
    /// Fails when the credential thread can't be started or stops during the load.
    pub(crate) fn spawn<S: CredentialStore + 'static>(
        worker: Worker<S>,
        net: Net,
    ) -> std::io::Result<Self> {
        Self::spawn_with_github(worker, net, github::GitHub::new())
    }

    #[cfg(test)]
    pub(crate) fn spawn_unconfigured<S: CredentialStore + 'static>(
        worker: Worker<S>,
        net: Net,
    ) -> std::io::Result<Self> {
        Self::spawn_with_github(worker, net, github::GitHub::with_client(None))
    }

    fn spawn_with_github<S: CredentialStore + 'static>(
        worker: Worker<S>,
        net: Net,
        github: github::GitHub,
    ) -> std::io::Result<Self> {
        let (state, started) = AccountState::start();
        let (events_tx, events) = mpsc::unbounded_channel();
        let (loaded_tx, loaded) = std_mpsc::channel();
        let (reads_tx, reads) = mpsc::unbounded_channel();
        let (jobs, released) = spawn(worker, loaded_tx, events_tx.clone(), reads_tx.clone())?;
        let salt = getrandom::u64().map_err(std::io::Error::other)?;
        let mut accounts = Self {
            github,
            state,
            jobs: Some(jobs),
            released: Some(released),
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
        };
        accounts.apply(started);
        let (done, key) = loaded
            .recv()
            .map_err(|_| std::io::Error::other("the credential thread stopped"))?;
        // No shell is connected yet: only the work it starts matters, not what it would say.
        accounts.handle(done);
        accounts.start_key(key);
        accounts
            .github
            .start(accounts.jobs.as_ref(), &accounts.reads_tx);
        Ok(accounts)
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
    /// account becomes usable, and again on Retry or a connection while no read succeeded.
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
            Event::Read { epoch, result } => return self.read_ended(epoch, result),
        };
        let ask = matches!(input, Input::Op(AccountOp::Retry) | Input::Connected);
        // A new shell hears the list there is.
        self.catalog.unsent |= matches!(input, Input::Connected) && self.catalog.listing.is_some();
        let was_usable = self.catalog.usable;
        let effects = self.state.handle(input);
        let mut frames = self.apply(effects);
        let catalog = &self.catalog;
        if catalog.usable
            && catalog.read.is_none()
            && catalog.listing.is_none()
            && (ask || !was_usable)
        {
            let (wait, access) = self.access();
            frames.extend(access);
            let http = Arc::clone(&self.net.http);
            let task = tokio::spawn(read_catalog(wait, http, self.reads_tx.clone()));
            self.catalog.read = Some(task.abort_handle());
            drop(tokio::spawn(catalog_task(
                self.catalog.epoch,
                task,
                self.reads_tx.clone(),
            )));
        }
        frames
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
                Vec::new()
            }
            Err(None) => Vec::new(),
            Err(Some(error)) => {
                let unavailable = error.key() == ErrorKey::PlanUnavailable;
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
        let error = |key, action| {
            UserError::new(key, Vec::new(), vec![action]).expect("one action is within the limit")
        };
        let reason = match self.state.view() {
            AccountView::SignedIn {
                plan: PlanState::Enabled,
                recovery: None | Some(AccountRecovery::Retry),
                ..
            } => match self.state.connection() {
                Some(connection) => {
                    return Availability::Ready(Ready {
                        connection: ConnectionTag(connection),
                        models: self.models().unwrap_or_default().to_vec(),
                        #[cfg(any(test, debug_assertions))]
                        trusted_draw: false,
                    });
                }
                None => error(ErrorKey::SignInNeeded, Action::SignIn),
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

    /// The account's catalog epoch while it can send model requests: its generation for jobs.
    pub(super) fn sendable(&self) -> Option<u64> {
        self.catalog.usable.then_some(self.catalog.epoch)
    }

    /// The last catalog read in this epoch, if one succeeded.
    pub(super) fn models(&self) -> Option<&[ModelInfo]> {
        self.catalog
            .listing
            .as_ref()
            .map(|listing| listing.models.as_slice())
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
                    frames.push(ControlToShell::Account(view));
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
                    if let Some(listener) = self.listener.take() {
                        listener.abort();
                    }
                    self.listener = Some(tokio::spawn(listen(
                        attempt,
                        host,
                        saved,
                        intent,
                        Arc::clone(&self.net.launcher),
                        self.events_tx.clone(),
                    )));
                }
                Effect::Close => {
                    if let Some(listener) = self.listener.take() {
                        listener.abort();
                    }
                }
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
    /// start counts it as uncertainty.
    pub(crate) async fn close(&mut self, deadline: Instant, refresh_by: Instant) {
        self.github
            .stop(self.jobs.as_ref(), &self.reads_tx, &self.net.http);
        if let Some(listener) = self.listener.take() {
            listener.abort();
        }
        self.catalog.cancel();
        self.connections.cancel();
        let settled = tokio::time::timeout_at(refresh_by, async {
            while self.state.refresh_out() || self.github.rotating() {
                let input = self.next().await;
                // No shell hears it: the brain is closing.
                self.handle(input);
            }
        })
        .await;
        if settled.is_err() {
            tracing::warn!("a refresh is still out at shutdown; its token stays marked as sent");
            self.github.stop_rotation();
        }
        // One the settling started.
        self.catalog.cancel();
        while self.github.pending() {
            let input = self.next().await;
            self.handle(input);
        }
        self.jobs = None;
        if let Some(mut released) = self.released.take()
            && tokio::time::timeout_at(deadline, &mut released)
                .await
                .is_err()
        {
            // It can't be aborted, and the instance capability is deleted next, so it ends first.
            tracing::warn!("a credential job is still running at the deadline; waiting for it");
            released.await.ok();
        }
        while self.github.verifying() {
            let verified = self.github.next_verification().await;
            self.handle(Event::Verification(verified));
        }
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
        let read = matches!(event, Event::Read { .. });
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
    mut worker: Worker<S>,
    loaded: std_mpsc::Sender<(Input, super::connections::KeyResult)>,
    events: mpsc::UnboundedSender<Input>,
    reads: mpsc::UnboundedSender<Event>,
) -> std::io::Result<(std_mpsc::Sender<Work>, oneshot::Receiver<()>)> {
    let (jobs, queued) = std_mpsc::channel::<Work>();
    let (released_tx, released) = oneshot::channel();
    thread::Builder::new()
        .name("credentials".to_owned())
        .spawn(move || {
            let mut loaded = Some(loaded);
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
                let done = Input::Vault(worker.run(job));
                let sent = match loaded.take() {
                    Some(loaded) => loaded
                        .send((
                            done,
                            worker.anthropic(crate::connections::anthropic::KeyOp::Load),
                        ))
                        .is_ok(),
                    None => events.send(done).is_ok(),
                };
                if !sent {
                    break;
                }
            }
            drop(worker);
            // Nobody waiting on the release is fine.
            released_tx.send(()).ok();
        })?;
    Ok((jobs, released))
}

async fn listen(
    attempt: u64,
    host: Secret<Zeroizing<String>>,
    saved: Option<Secret<Account>>,
    intent: Intent,
    launcher: Arc<dyn Launcher + Send + Sync>,
    events: mpsc::UnboundedSender<Input>,
) {
    let result = async {
        let sign_in = SignIn::bind(host.expose(), saved.as_ref().map(Secret::expose), intent)
            .await
            .map_err(Listened::Bind)?;
        // Opening the browser is OS work that can block, so it runs off the core loop.
        let (sign_in, opened) = tokio::task::spawn_blocking(move || {
            let opened = sign_in.open_browser(launcher.as_ref());
            (sign_in, opened)
        })
        .await
        .map_err(|_| Listened::Launch(LaunchError::Launch))?;
        opened.map_err(Listened::Launch)?;
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

async fn revoke_task(
    entries: Vec<Secret<Revocable>>,
    http: HttpFactory,
    events: mpsc::UnboundedSender<Input>,
) {
    let deadline = Instant::now() + BUDGET;
    let confirmed = match tokio::task::spawn_blocking(move || http()).await {
        Ok(Ok(http)) => revoke(&http, entries, deadline).await,
        _ => Vec::new(),
    };
    events.send(Input::Revoked(confirmed)).ok();
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
    net: Net,
    events: mpsc::UnboundedSender<Input>,
) {
    let build = Arc::clone(&net.http);
    // Its own task, so one that panics still ends the flight; the payload isn't logged.
    let result = tokio::spawn(async move {
        let http = tokio::task::spawn_blocking(move || build())
            .await
            .map_err(|_| ExchangeError::Http(HttpError::Configuration))?
            .map_err(ExchangeError::Http)?;
        refresh(&http, client.expose(), refresh_token.expose()).await
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
