//! The account system on the brain's side: it runs the account machine's effects. Credential
//! jobs go to one thread that owns the vault, so every Credential Manager read and write is
//! serialized; Windows can miss a read on one thread while another writes. The listener and
//! the exchange run as tasks, and everything reports back on one channel the core loop polls.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::thread;

use kumochi_protocol::Secret;
use kumochi_protocol::message::ControlToShell;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use zeroize::Zeroizing;

use crate::connections::account::{
    AccountState, Denied, Effect, Input, Job, Listened, Received, Worker,
};
use crate::connections::clock::Clock;
use crate::connections::http_client::{HttpError, SharedHttp};
use crate::connections::oauth::exchange::{ExchangeError, exchange};
use crate::connections::oauth::id_token::{IdentityError, verify, verify_refreshed};
use crate::connections::oauth::refresh::refresh;
use crate::connections::oauth::revoke::{BUDGET, revoke};
use crate::connections::oauth::{Authorization, Intent, LaunchError, Launcher, SignIn};
use crate::connections::records::{Account, Revocable};
use crate::platform::credential::CredentialStore;

use super::{Brain, Full, Link};

/// What a caller of [`Accounts::access`] gets: the bearer, or why there's none.
pub(crate) type Access = Result<Secret<Zeroizing<String>>, Denied>;

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

/// The account machine, the credential thread and the tasks it started.
#[derive(Debug)]
pub(crate) struct Accounts {
    state: AccountState,
    jobs: Option<std_mpsc::Sender<Job>>,
    /// Fires once the credential thread has dropped the vault and its session lock.
    released: Option<oneshot::Receiver<()>>,
    events_tx: mpsc::UnboundedSender<Input>,
    events: mpsc::UnboundedReceiver<Input>,
    /// The live attempt's listener; aborting it closes the loopback.
    listener: Option<JoinHandle<()>>,
    net: Net,
    /// Callers waiting for the bearer, by ticket.
    tickets: HashMap<u64, oneshot::Sender<Access>>,
    next_ticket: u64,
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
        let (state, started) = AccountState::start();
        let (events_tx, events) = mpsc::unbounded_channel();
        let (loaded_tx, loaded) = std_mpsc::channel();
        let (jobs, released) = spawn(worker, loaded_tx, events_tx.clone())?;
        let mut accounts = Self {
            state,
            jobs: Some(jobs),
            released: Some(released),
            events_tx,
            events,
            listener: None,
            net,
            tickets: HashMap::new(),
            next_ticket: 0,
        };
        accounts.apply(started);
        let done = loaded
            .recv()
            .map_err(|_| std::io::Error::other("the credential thread stopped"))?;
        // No shell is connected yet: only the work it starts matters, not what it would say.
        accounts.handle(done);
        Ok(accounts)
    }

    /// The next report from the credential thread or a task.
    pub(crate) async fn next(&mut self) -> Input {
        self.events
            .recv()
            .await
            .expect("the account system holds a sender")
    }

    /// Asks for the bearer, refreshed first when it's due; it comes on the receiver. Dropping the
    /// receiver cancels only the wait: a refresh it started still runs to its end and is saved.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "PR 10b's catalog read is the first caller")
    )]
    pub(crate) fn access(&mut self) -> (oneshot::Receiver<Access>, Vec<ControlToShell>) {
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        let (answer, receiver) = oneshot::channel();
        self.tickets.insert(ticket, answer);
        let now = self.net.clock.unix_seconds();
        (receiver, self.handle(Input::Access { ticket, now }))
    }

    /// Applies an input; what the shell should hear, in order.
    pub(crate) fn handle(&mut self, input: Input) -> Vec<ControlToShell> {
        let effects = self.state.handle(input);
        self.apply(effects)
    }

    fn apply(&mut self, effects: Vec<Effect>) -> Vec<ControlToShell> {
        let mut frames = Vec::new();
        for effect in effects {
            match effect {
                Effect::View(view) => frames.push(ControlToShell::Account(view)),
                Effect::Error(error) => frames.push(ControlToShell::Error(error)),
                Effect::Vault(job) => {
                    let sent = self
                        .jobs
                        .as_ref()
                        .is_some_and(|jobs| jobs.send(job).is_ok());
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
                Effect::Access { ticket, result } => {
                    // A caller that stopped waiting just doesn't hear it.
                    if let Some(answer) = self.tickets.remove(&ticket) {
                        answer.send(result).ok();
                    }
                }
            }
        }
        frames
    }

    /// Stops taking work and waits for the credential job already dispatched, past `deadline` if
    /// it must, so it never overlaps the instance capability's delete; then the vault and its
    /// session lock are gone. Jobs not yet dispatched are dropped: a pending flag they would
    /// have cleared stays set, and the next start counts it as uncertainty.
    pub(crate) async fn close(&mut self, deadline: Instant) {
        if let Some(listener) = self.listener.take() {
            listener.abort();
        }
        self.jobs = None;
        let Some(mut released) = self.released.take() else {
            return;
        };
        if tokio::time::timeout_at(deadline, &mut released)
            .await
            .is_err()
        {
            // It can't be aborted, and the instance capability is deleted next, so it ends first.
            tracing::warn!("a credential job is still running at the deadline; waiting for it");
            released.await.ok();
        }
    }
}

impl Brain {
    /// Applies an account input and tells the connected shell, if any, what changed.
    pub(super) fn account_input(
        &mut self,
        input: Input,
        link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        let frames = self.account.handle(input);
        let Some(link) = link else {
            return Ok(());
        };
        frames.iter().try_for_each(|frame| link.control(frame))
    }
}

/// The credential thread: runs each job whole, in order, and reports it; the first, the startup
/// load, to `loaded`.
fn spawn<S: CredentialStore + 'static>(
    mut worker: Worker<S>,
    loaded: std_mpsc::Sender<Input>,
    events: mpsc::UnboundedSender<Input>,
) -> std::io::Result<(std_mpsc::Sender<Job>, oneshot::Receiver<()>)> {
    let (jobs, queued) = std_mpsc::channel::<Job>();
    let (released_tx, released) = oneshot::channel();
    thread::Builder::new()
        .name("credentials".to_owned())
        .spawn(move || {
            let mut loaded = Some(loaded);
            for job in queued {
                let done = Input::Vault(worker.run(job));
                let sent = match loaded.take() {
                    Some(loaded) => loaded.send(done).is_ok(),
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

async fn refresh_task(
    generation: u64,
    client: Secret<Zeroizing<String>>,
    refresh_token: Secret<Zeroizing<String>>,
    net: Net,
    events: mpsc::UnboundedSender<Input>,
) {
    let result = async {
        let build = Arc::clone(&net.http);
        let http = tokio::task::spawn_blocking(move || build())
            .await
            .map_err(|_| ExchangeError::Http(HttpError::Configuration))?
            .map_err(ExchangeError::Http)?;
        refresh(&http, client.expose(), refresh_token.expose()).await
    }
    .await;
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
    let result = async {
        let build = Arc::clone(&net.http);
        let http = tokio::task::spawn_blocking(move || build())
            .await
            .map_err(|_| IdentityError::Http(HttpError::Configuration))??;
        verify_refreshed(
            &http,
            &id_token,
            client.expose(),
            subject.expose(),
            net.clock.as_ref(),
        )
        .await
    }
    .await;
    events.send(Input::Validated { generation, result }).ok();
}

pub(super) async fn exchange_task(
    attempt: u64,
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
        })
    }
    .await;
    events.send(Input::Exchanged { attempt, result }).ok();
}
