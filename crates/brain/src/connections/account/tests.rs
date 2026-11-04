//! The account machine driven against the real vault on an in-memory store. Credential jobs run
//! synchronously, so every durable write's marker is observed, and faults land on exact
//! mutations. Exchange results are fed in as data: nothing touches a network.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use kumochi_protocol::Secret;
use kumochi_protocol::constants::SESSION_LOCK;
use kumochi_protocol::message::{AccountRecovery, Action, Cleanup, ErrorKey, UserError};
use zeroize::Zeroizing;

use super::*;
use crate::connections::clock::{Clock, FixedClock};
use crate::connections::error::VaultError;
use crate::connections::records::{
    MAX_ENTRY_BYTES, MAX_REFRESH_BYTES, MAX_TOKEN_BYTES, Origin, Revocable,
};
use crate::connections::vault::fake::{FakeStore, Fault, Mutation};
use crate::connections::vault::{SaveResult, Vault};
use crate::platform::SessionLock;
use crate::test_support::{Captured, Scratch};

mod lane;
mod ownership;
mod refresh;
mod sign_out;
mod sweep;

const NOW: u64 = 1_800_000_000;
const EMAIL: &str = "person@example.invalid";
const PLAN: &str = "openid offline_access chatgpt.tokens.use.direct";

/// The marker's exact JSON with no sign-out pending and the named flags set.
#[expect(
    clippy::unnecessary_wraps,
    reason = "compared with markers read from the store, which may be absent"
)]
fn marker(set: &[&str]) -> Option<String> {
    const FLAGS: [&str; 3] = ["sweep", "request_pending", "outcome_unknown"];
    assert!(set.iter().all(|name| FLAGS.contains(name)), "{set:?}");
    let flag = |name| set.contains(&name);
    let (sweep, pending, unknown) = (
        flag("sweep"),
        flag("request_pending"),
        flag("outcome_unknown"),
    );
    Some(format!(
        r#"{{"v":1,"sign_out":null,"sweep":{sweep},"request_pending":{pending},"outcome_unknown":{unknown},"notice":false}}"#
    ))
}

fn signed_out() -> AccountView {
    AccountView::SignedOut {
        revocation_unconfirmed: false,
    }
}

fn signed_in(plan: PlanState, recovery: Option<AccountRecovery>) -> AccountView {
    AccountView::SignedIn {
        email: Some(AccountEmail::new(EMAIL).expect("valid")),
        plan,
        recovery,
    }
}

fn user_error(key: ErrorKey, actions: &[Action]) -> UserError {
    UserError::new(key, Vec::new(), actions.to_vec()).expect("within limits")
}

fn secret(text: &str) -> Secret<Zeroizing<String>> {
    Secret::new(Zeroizing::new(text.to_owned()))
}

fn authorization(client: &str, code: &str) -> Authorization {
    Authorization {
        code: secret(code),
        client_id: secret(client),
        nonce: secret("nonce"),
        verifier: secret(&"v".repeat(43)),
        redirect: "http://127.0.0.1:1455/auth/callback".to_owned(),
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "an exchange's result, as the reducer takes it"
)]
fn received(refresh: Option<&str>, subject: &str, scope: &str) -> Result<Received, ExchangeError> {
    Ok(Received {
        tokens: TokenSet {
            access_token: secret("access"),
            refresh_token: refresh.map(secret),
            id_token: Some(secret("identity")),
            scope: secret(scope),
            expires_in: 3600,
        },
        identity: Ok(VerifiedIdentity {
            subject: secret(subject),
            email: Some(secret(EMAIL)),
        }),
        now: Some(NOW),
        connection: 1,
    })
}

fn saved_account(subject: &str, refresh: &str) -> Secret<Account> {
    Secret::new(Account {
        client_id: Zeroizing::new("client-saved".to_owned()),
        subject: Some(Zeroizing::new(subject.to_owned())),
        email: Some(Zeroizing::new(EMAIL.to_owned())),
        plan_enabled: true,
        tokens: Some(Tokens {
            access_token: Zeroizing::new("old-access".to_owned()),
            refresh_token: Zeroizing::new(refresh.to_owned()),
            id_token: None,
            scope: Zeroizing::new(PLAN.to_owned()),
            expires_at: NOW + 60,
            pending_identity_validation: false,
            refresh_sent: false,
            connection: 0,
        }),
    })
}

/// Wall and monotonic time a test moves: together as a running clock does, or the wall alone
/// as a jump does.
#[derive(Clone)]
struct Wall {
    unix: Arc<AtomicU64>,
    ran: Arc<AtomicU64>,
    start: Instant,
}

impl Wall {
    fn new() -> Self {
        Self {
            unix: Arc::new(AtomicU64::new(NOW)),
            ran: Arc::default(),
            start: Instant::now(),
        }
    }
}

impl Clock for Wall {
    fn unix_seconds(&self) -> Option<u64> {
        Some(self.unix.load(Ordering::SeqCst))
    }

    fn monotonic(&self) -> Instant {
        self.start + Duration::from_secs(self.ran.load(Ordering::SeqCst))
    }
}

fn committed(result: &SaveResult) {
    assert!(
        matches!(result, SaveResult::Committed { leftovers: false }),
        "{result:?}"
    );
}

/// The machine, the vault worker it drives, and everything it told the runtime.
struct Harness {
    state: AccountState,
    worker: Worker<FakeStore>,
    store: FakeStore,
    service: String,
    scratch: Scratch,
    wall: Wall,
    views: Vec<AccountView>,
    errors: Vec<UserError>,
    /// The persisted marker after every credential job, in order.
    markers: Vec<Option<String>>,
    listening: Vec<(u64, Intent)>,
    /// Each listener's saved client, in order.
    saved_clients: Vec<Option<String>>,
    /// Every refresh sent: its generation, client and refresh token.
    refreshes: Vec<(u64, String, String)>,
    /// Every identity check of a rotated set: its generation, subject and token.
    validations: Vec<(u64, String, String)>,
    /// Every answer to `Access`, with the bearer's text.
    answers: Vec<(u64, Result<String, Denied>)>,
    closed: usize,
    /// Every token request sent, in order.
    sent: Vec<(u64, Authorization)>,
    /// Every revocation run asked for, in order.
    revoking: Vec<Vec<Secret<Revocable>>>,
    replied: usize,
    /// Hold the next credential job instead of running it.
    hold: bool,
    held: Option<Job>,
}

impl Harness {
    fn new(name: &str) -> Self {
        Self::seeded(name, |_| {})
    }

    fn seeded(name: &str, seed: impl FnOnce(&mut Vault<FakeStore>)) -> Self {
        let scratch = Scratch::new(name);
        let service = format!("test-account-{:016x}", getrandom::u64().expect("random"));
        let store = FakeStore::default();
        let seeding = scratch.db().with_file_name("seed.lock");
        seed(&mut Vault::with_service(store.clone(), &service, &seeding).expect("seed vault"));
        Self::boot(scratch, service, store, Wall::new())
    }

    fn boot(scratch: Scratch, service: String, store: FakeStore, wall: Wall) -> Self {
        let lock = scratch.db().with_file_name(SESSION_LOCK);
        let (opener, name) = (store.clone(), service.clone());
        let worker = Worker::new(
            Box::new(move || Vault::with_service(opener.clone(), &name, &lock)),
            Arc::new(wall.clone()),
        );
        let (state, effects) = AccountState::start();
        let mut harness = Self {
            state,
            worker,
            store,
            service,
            scratch,
            wall,
            views: Vec::new(),
            errors: Vec::new(),
            markers: Vec::new(),
            listening: Vec::new(),
            saved_clients: Vec::new(),
            refreshes: Vec::new(),
            validations: Vec::new(),
            answers: Vec::new(),
            closed: 0,
            sent: Vec::new(),
            revoking: Vec::new(),
            replied: 0,
            hold: false,
            held: None,
        };
        harness.absorb(effects);
        harness
    }

    /// A process death: everything in memory is gone, and a new brain reads what's on disk.
    fn restart(self) -> Self {
        let Self {
            scratch,
            service,
            store,
            worker,
            wall,
            ..
        } = self;
        drop(worker);
        Self::boot(scratch, service, store, wall)
    }

    /// Time passes.
    fn advance(&self, seconds: u64) {
        self.wall.ran.fetch_add(seconds, Ordering::SeqCst);
        self.jump(seconds);
    }

    /// Only the wall clock moves.
    fn jump(&self, seconds: u64) {
        self.wall.unix.fetch_add(seconds, Ordering::SeqCst);
    }

    fn feed(&mut self, input: Input) {
        let effects = self.state.handle(input);
        self.absorb(effects);
    }

    fn op(&mut self, op: AccountOp) {
        self.feed(Input::Op(op));
    }

    /// Records what the machine said, then runs its job: the lane's thread reports a job only
    /// after the effects that came with it went out.
    fn absorb(&mut self, effects: Vec<Effect>) {
        let mut job = None;
        for effect in effects {
            match effect {
                Effect::View(view) => self.views.push(view),
                Effect::Error(error) => self.errors.push(error),
                Effect::Vault(job) if self.hold => {
                    assert!(self.held.replace(job).is_none(), "one job in flight");
                    self.hold = false;
                }
                Effect::Vault(next) => assert!(job.replace(next).is_none(), "one job in flight"),
                Effect::Listen {
                    attempt,
                    intent,
                    saved,
                    ..
                } => {
                    self.listening.push((attempt, intent));
                    self.saved_clients
                        .push(saved.map(|account| account.expose().client_id.to_string()));
                }
                Effect::Close => self.closed += 1,
                Effect::Exchange {
                    attempt,
                    authorization,
                } => self.sent.push((attempt, authorization)),
                Effect::Revoke(entries) => self.revoking.push(entries),
                Effect::Refresh {
                    generation,
                    client,
                    refresh_token,
                } => self.refreshes.push((
                    generation,
                    client.expose().to_string(),
                    refresh_token.expose().to_string(),
                )),
                Effect::Validate {
                    generation,
                    subject,
                    id_token,
                    ..
                } => self.validations.push((
                    generation,
                    subject.expose().to_string(),
                    id_token.expose().to_string(),
                )),
                Effect::Access { ticket, result } => self
                    .answers
                    .push((ticket, result.map(|bearer| bearer.expose().to_string()))),
            }
        }
        if let Some(job) = job {
            self.run(job);
        }
    }

    fn run(&mut self, job: Job) {
        // Jobs run synchronously here, so a job that queues another forever would recurse.
        assert!(self.markers.len() < 64, "runaway credential jobs");
        let done = self.worker.run(job);
        self.markers.push(self.marker());
        self.feed(Input::Vault(done));
    }

    fn release(&mut self) {
        let job = self.held.take().expect("a held job");
        self.run(job);
    }

    fn marker(&self) -> Option<String> {
        let state = self.store.0.lock().expect("fake lock");
        let bytes = state.values.get(&format!("{}/signout", self.service))?;
        Some(String::from_utf8(bytes.expose().to_vec()).expect("JSON"))
    }

    fn callback(&mut self, client: &str) {
        let (attempt, _) = *self.listening.last().expect("listening");
        let code = format!("code-{attempt}");
        self.feed(Input::Listened {
            attempt,
            result: Listened::Authorized(authorization(client, &code)),
        });
    }

    fn reply(&mut self, result: Result<Received, ExchangeError>) {
        let attempt = self.sent[self.replied].0;
        self.replied += 1;
        self.feed(Input::Exchanged { attempt, result });
    }

    fn reader(&self) -> Vault<FakeStore> {
        let lock = self.scratch.db().with_file_name("reader.lock");
        Vault::with_service(self.store.clone(), &self.service, &lock).expect("reader")
    }

    fn account(&self) -> Option<Account> {
        let account = self.reader().load_account().expect("readable account");
        account.map(|account| account.expose().clone())
    }

    fn refresh_token(&self) -> Option<String> {
        let account = self.account()?;
        Some(account.tokens?.refresh_token.to_string())
    }

    fn revocations(&self) -> Vec<(String, String, u64, Origin)> {
        let list = self.reader().load_revocations().expect("readable list");
        list.map(|list| {
            list.expose()
                .entries
                .iter()
                .map(|entry| {
                    (
                        entry.client_id.to_string(),
                        entry.refresh_token.to_string(),
                        entry.added_at,
                        entry.origin,
                    )
                })
                .collect()
        })
        .unwrap_or_default()
    }

    fn view(&self) -> &AccountView {
        self.views.last().expect("a view")
    }
}

/// A sign-in through a new registration: the issued client is saved before the code is sent,
/// the pending flag is written before the request and cleared by the commit.
#[test]
fn a_first_sign_in_registers_then_commits_and_releases_the_lane() {
    let mut h = Harness::new("account-first");
    assert_eq!(h.views, [signed_out()]);
    h.op(AccountOp::SignIn);
    assert_eq!(h.view(), &AccountView::SigningIn);
    h.callback("client-issued");
    assert_eq!(h.sent.len(), 1, "the code is sent once");
    let registration = h.account().expect("registration saved before the exchange");
    assert_eq!(registration.client_id.as_str(), "client-issued");
    assert!(registration.tokens.is_none());
    h.reply(received(Some("refresh-new"), "subject-1", PLAN));
    assert_eq!(h.view(), &signed_in(PlanState::Enabled, None));
    assert!(h.errors.is_empty(), "{:?}", h.errors);
    // Load, registration, pending flag, commit.
    assert_eq!(h.markers, [None, None, marker(&["request_pending"]), None]);
    let account = h.account().expect("account");
    assert_eq!(
        account.subject.as_deref().map(String::as_str),
        Some("subject-1")
    );
    let tokens = account.tokens.expect("tokens");
    assert_eq!(tokens.refresh_token.as_str(), "refresh-new");
    assert_eq!(tokens.expires_at, NOW + 3600);
    assert!(h.revocations().is_empty());
}

#[test]
fn a_different_subject_is_refused_and_its_set_goes_to_the_revocation_list() {
    let mut h = Harness::seeded("account-other", |vault| {
        committed(&vault.save_account(&saved_account("subject-1", "refresh-old")));
    });
    assert_eq!(h.view(), &signed_in(PlanState::Enabled, None));
    h.op(AccountOp::SignIn);
    h.callback("client-saved");
    h.reply(received(Some("refresh-other"), "subject-2", PLAN));
    assert_eq!(
        h.errors,
        [user_error(ErrorKey::SignInOtherAccount, &[Action::SignIn])]
    );
    assert_eq!(
        h.view(),
        &signed_in(PlanState::Enabled, None),
        "the old account stays live"
    );
    assert_eq!(h.refresh_token().as_deref(), Some("refresh-old"));
    assert_eq!(
        h.revocations(),
        [(
            "client-saved".to_owned(),
            "refresh-other".to_owned(),
            NOW,
            Origin::Exchange
        )]
    );
    // Pending flag, then the list save: sweep is set first and the flag cleared after.
    assert_eq!(
        h.markers[1..],
        [marker(&["request_pending"]), marker(&["sweep"])]
    );
}

#[test]
fn a_typed_oauth_refusal_issued_nothing_and_clears_the_pending_flag() {
    let cases = [
        (
            ExchangeError::OAuth {
                status: 400,
                code: OAuthCode::InvalidGrant,
            },
            user_error(ErrorKey::SignInFailed, &[Action::SignIn]),
        ),
        (
            ExchangeError::OAuth {
                status: 401,
                code: OAuthCode::InvalidClient,
            },
            user_error(ErrorKey::AccountMisconfigured, &[]),
        ),
        (
            ExchangeError::Http(HttpError::TrustStoreUnavailable),
            user_error(ErrorKey::TrustStoreUnavailable, &[Action::SignIn]),
        ),
    ];
    for (failure, expected) in cases {
        let mut h = Harness::new("account-refused");
        h.op(AccountOp::SignIn);
        h.callback("client-issued");
        h.reply(Err(failure));
        assert_eq!(h.errors, std::slice::from_ref(&expected));
        assert_eq!(h.view(), &signed_out());
        assert_eq!(
            h.markers[2..],
            [marker(&["request_pending"]), None],
            "{expected:?}"
        );
        assert!(h.revocations().is_empty());
    }
}

#[test]
fn an_ambiguous_outcome_is_kept_and_a_later_success_never_clears_it() {
    let cases = [
        (
            ExchangeError::Response { status: 503 },
            ErrorKey::AccountOffline,
        ),
        (
            ExchangeError::Http(HttpError::Timeout),
            ErrorKey::AccountOffline,
        ),
        // A 4xx whose body isn't an OAuth error object, such as a proxy page.
        (
            ExchangeError::Response { status: 403 },
            ErrorKey::SignInFailed,
        ),
        (ExchangeError::Bounds, ErrorKey::SignInFailed),
        (
            ExchangeError::Unusable {
                refresh_token: Some(secret("")),
            },
            ErrorKey::SignInFailed,
        ),
        (
            ExchangeError::Unusable {
                refresh_token: Some(secret(&"r".repeat(MAX_REFRESH_BYTES + 1))),
            },
            ErrorKey::SignInFailed,
        ),
    ];
    for (failure, key) in cases {
        let mut h = Harness::new("account-ambiguous");
        h.op(AccountOp::SignIn);
        h.callback("client-issued");
        h.reply(Err(failure));
        assert_eq!(h.errors, [user_error(key, &[Action::SignIn])]);
        assert_eq!(h.marker(), marker(&["outcome_unknown"]), "{key:?}");
        h.op(AccountOp::SignIn);
        h.callback("client-issued");
        h.reply(received(Some("refresh-new"), "subject-1", PLAN));
        assert_eq!(h.view(), &signed_in(PlanState::Enabled, None));
        assert_eq!(h.marker(), marker(&["outcome_unknown"]), "{key:?}");
    }
}

#[test]
fn an_unusable_response_that_named_a_refresh_token_lists_it() {
    let mut h = Harness::new("account-unusable");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.reply(Err(ExchangeError::Unusable {
        refresh_token: Some(secret("refresh-unusable")),
    }));
    assert_eq!(
        h.errors,
        [user_error(ErrorKey::SignInFailed, &[Action::SignIn])]
    );
    assert_eq!(h.view(), &signed_out());
    assert_eq!(
        h.revocations(),
        [(
            "client-issued".to_owned(),
            "refresh-unusable".to_owned(),
            NOW,
            Origin::Exchange
        )]
    );
    assert_eq!(h.marker(), marker(&["sweep"]));
}

/// CAPS-116: a set whose tokens fit their field bounds but escape past what a record holds is
/// refused where it arrives, so no save that can never land is offered for Retry.
#[test]
fn a_set_too_long_once_escaped_is_refused_where_it_arrives() {
    // 4,858 escaped backslashes and an `r` are 9,717 bytes, and the frame
    // {"client_id":"client-issued","refresh_token":"","added_at":u64::MAX,"origin":"Exchange"}
    // is 100, so this entry is exactly the bound.
    assert_eq!(MAX_ENTRY_BYTES, 9_817);
    let fits = format!("{}r", "\\".repeat(4_858));
    let over = format!("{fits}r");
    assert!(over.len() <= MAX_REFRESH_BYTES, "within the field bound");

    let mut h = Harness::new("account-escaped-fits");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.reply(received(Some(&fits), "subject-1", PLAN));
    assert_eq!(h.view(), &signed_in(PlanState::Enabled, None));
    assert_eq!(h.refresh_token(), Some(fits));

    for reply in [
        received(Some(&over), "subject-1", PLAN),
        Err(ExchangeError::Unusable {
            refresh_token: Some(secret(&over)),
        }),
    ] {
        let mut h = Harness::new("account-escaped-over");
        h.op(AccountOp::SignIn);
        h.callback("client-issued");
        h.reply(reply);
        assert_eq!(
            h.errors,
            [user_error(ErrorKey::SignInFailed, &[Action::SignIn])]
        );
        assert_eq!(h.view(), &signed_out());
        assert_eq!(h.marker(), marker(&["outcome_unknown"]));
        assert_eq!(h.refresh_token(), None);
        assert!(h.revocations().is_empty(), "nothing owns it");
    }

    let mut h = Harness::new("account-escaped-access");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    let mut reply = received(Some("refresh-new"), "subject-1", PLAN);
    reply.as_mut().expect("a set").tokens.access_token = secret(&"\0".repeat(MAX_TOKEN_BYTES));
    h.reply(reply);
    assert_eq!(
        h.errors,
        [user_error(ErrorKey::SignInFailed, &[Action::SignIn])]
    );
    assert_eq!(h.refresh_token(), None);
    assert_eq!(
        h.revocations(),
        [(
            "client-issued".to_owned(),
            "refresh-new".to_owned(),
            NOW,
            Origin::Exchange
        )]
    );
}

#[test]
fn a_request_unresolved_at_a_crash_becomes_uncertainty_on_the_next_request() {
    let mut h = Harness::new("account-crash");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    assert_eq!(h.sent.len(), 1);
    assert_eq!(h.marker(), marker(&["request_pending"]));
    let mut h = h.restart();
    assert_eq!(h.view(), &signed_out());
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    assert_eq!(
        h.marker(),
        marker(&["request_pending", "outcome_unknown"]),
        "the old flag folds into uncertainty"
    );
    h.reply(received(Some("refresh-new"), "subject-1", PLAN));
    assert_eq!(h.view(), &signed_in(PlanState::Enabled, None));
    assert_eq!(h.marker(), marker(&["outcome_unknown"]));
}

#[test]
fn a_grant_without_a_refresh_token_is_refused_and_owns_nothing() {
    let mut h = Harness::new("account-no-refresh");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.reply(received(None, "subject-1", PLAN));
    assert_eq!(
        h.errors,
        [user_error(ErrorKey::SignInFailed, &[Action::SignIn])]
    );
    assert_eq!(h.view(), &signed_out());
    assert_eq!(h.marker(), None);
    assert!(h.account().expect("registration").tokens.is_none());
    assert!(h.revocations().is_empty());
}

#[test]
fn a_replacing_sign_in_waits_for_the_sent_exchange_whose_set_is_listed() {
    let mut h = Harness::new("account-replace");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    assert_eq!(h.sent.len(), 1);
    h.op(AccountOp::SignIn);
    assert_eq!(h.listening.len(), 2, "the replacement listens at once");
    h.callback("client-issued");
    assert_eq!(h.sent.len(), 1, "one request at a time");
    h.reply(received(Some("refresh-first"), "subject-1", PLAN));
    assert_eq!(
        h.sent.len(),
        2,
        "the replacement's code is sent once the first is owned"
    );
    assert_eq!(h.sent[1].0, h.listening[1].0);
    assert_eq!(h.view(), &AccountView::SigningIn);
    assert!(h.errors.is_empty(), "a replaced attempt ends quietly");
    h.reply(received(Some("refresh-second"), "subject-1", PLAN));
    assert_eq!(h.view(), &signed_in(PlanState::Enabled, None));
    assert_eq!(h.refresh_token().as_deref(), Some("refresh-second"));
    assert_eq!(
        h.revocations(),
        [(
            "client-issued".to_owned(),
            "refresh-first".to_owned(),
            NOW,
            Origin::Exchange
        )]
    );
}

/// The runtime delivers one result per request; a repeated one must not take the next request.
#[test]
fn a_repeated_exchange_result_leaves_the_next_request_to_finish() {
    let mut h = Harness::new("account-repeat-sent");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.reply(received(Some("refresh-first"), "subject-1", PLAN));
    let first = h.sent[0].0;
    h.feed(Input::Exchanged {
        attempt: first,
        result: received(Some("refresh-again"), "subject-1", PLAN),
    });
    assert_eq!(h.view(), &AccountView::SigningIn);
    h.reply(received(Some("refresh-second"), "subject-1", PLAN));
    assert_eq!(h.view(), &signed_in(PlanState::Enabled, None));
    assert_eq!(h.refresh_token().as_deref(), Some("refresh-second"));
    assert_eq!(
        h.revocations(),
        [(
            "client-issued".to_owned(),
            "refresh-first".to_owned(),
            NOW,
            Origin::Exchange
        )]
    );
}

#[test]
fn a_repeated_exchange_result_keeps_the_owned_set() {
    let mut h = Harness::new("account-repeat-owning");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.hold = true;
    h.reply(received(Some("refresh-first"), "subject-1", PLAN));
    let attempt = h.sent[0].0;
    h.feed(Input::Exchanged {
        attempt,
        result: received(Some("refresh-again"), "subject-1", PLAN),
    });
    h.release();
    assert_eq!(h.view(), &signed_in(PlanState::Enabled, None));
    assert_eq!(h.refresh_token().as_deref(), Some("refresh-first"));
    assert_eq!(h.revocations(), []);
}

#[test]
fn a_replaced_attempts_waiting_code_is_never_sent() {
    let mut h = Harness::new("account-never-sent");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.reply(received(Some("refresh-first"), "subject-1", PLAN));
    let sent: Vec<u64> = h.sent.iter().map(|(attempt, _)| *attempt).collect();
    assert_eq!(sent, [h.listening[0].0, h.listening[2].0]);
    assert_eq!(h.sent[1].1.code.expose().as_str(), "code-3");
    // Load, registration, the first flag, the first set listed (its attempt was replaced),
    // then the third attempt's flag: the second code never takes the lane.
    assert_eq!(
        h.markers,
        [
            None,
            None,
            marker(&["request_pending"]),
            marker(&["sweep"]),
            marker(&["sweep", "request_pending"])
        ]
    );
}

#[test]
fn cancel_closes_the_listener_and_a_late_callback_does_nothing() {
    let mut h = Harness::new("account-cancel");
    h.op(AccountOp::SignIn);
    h.op(AccountOp::CancelSignIn);
    assert_eq!(h.closed, 1);
    assert_eq!(h.view(), &signed_out());
    let jobs = h.markers.len();
    h.feed(Input::Listened {
        attempt: h.listening[0].0,
        result: Listened::Authorized(authorization("client-issued", "late")),
    });
    assert_eq!(h.markers.len(), jobs, "no registration is saved");
    assert!(h.sent.is_empty());
    assert!(h.errors.is_empty());
}

#[test]
fn a_cancelled_attempts_sent_exchange_lands_in_the_revocation_list() {
    let mut h = Harness::seeded("account-cancel-sent", |vault| {
        committed(&vault.save_account(&saved_account("subject-1", "refresh-old")));
    });
    h.op(AccountOp::SignIn);
    h.callback("client-saved");
    h.op(AccountOp::CancelSignIn);
    assert_eq!(h.view(), &signed_in(PlanState::Enabled, None));
    h.reply(received(Some("refresh-cancelled"), "subject-1", PLAN));
    assert!(h.errors.is_empty());
    assert_eq!(h.refresh_token().as_deref(), Some("refresh-old"));
    assert_eq!(h.revocations().len(), 1);
    assert_eq!(h.revocations()[0].1, "refresh-cancelled");
    assert_eq!(h.marker(), marker(&["sweep"]));
}

#[test]
fn listener_endings_map_to_their_errors_and_declining_is_quiet() {
    let cases = [
        (Listened::Receive(SignInError::Declined), None),
        (
            Listened::Receive(SignInError::Expired),
            Some(ErrorKey::SignInTimedOut),
        ),
        (
            Listened::Launch(LaunchError::Launch),
            Some(ErrorKey::BrowserUnavailable),
        ),
        (Listened::Bind(BindError::Io), Some(ErrorKey::SignInFailed)),
        (
            Listened::Receive(SignInError::Authorization),
            Some(ErrorKey::SignInFailed),
        ),
    ];
    for (result, key) in cases {
        let mut h = Harness::new("account-listened");
        h.op(AccountOp::SignIn);
        h.feed(Input::Listened {
            attempt: h.listening[0].0,
            result,
        });
        let expected: Vec<_> = key
            .map(|key| user_error(key, &[Action::SignIn]))
            .into_iter()
            .collect();
        assert_eq!(h.errors, expected);
        assert_eq!(h.view(), &signed_out());
        assert!(h.sent.is_empty());
    }
}

/// Design §2: tokens, the code, the client id, the subject, the email and URLs never reach the
/// logs or any `Debug` output, through an ambiguous failure, a refused save and a list save.
#[test]
fn no_secret_reaches_the_logs_or_debug_output() {
    let captured = Captured::default();
    let guard = captured.install();
    let authorization = || Authorization {
        code: secret("code-SENTINEL"),
        client_id: secret("client-SENTINEL"),
        nonce: secret("nonce-SENTINEL"),
        verifier: secret(&format!("verifier-SENTINEL-{}", "v".repeat(30))),
        // The loopback redirect carries no secret and isn't redacted; the launch URL, which
        // does, never reaches the machine.
        redirect: "http://127.0.0.1:1455/auth/callback".to_owned(),
    };
    let issued = || {
        Ok(Received {
            tokens: TokenSet {
                access_token: secret("access-SENTINEL"),
                refresh_token: Some(secret("refresh-SENTINEL")),
                id_token: Some(secret("idtoken-SENTINEL")),
                scope: secret(PLAN),
                expires_in: 3600,
            },
            identity: Ok(VerifiedIdentity {
                subject: secret("subject-SENTINEL"),
                email: Some(secret("SENTINEL@example.invalid")),
            }),
            now: Some(NOW),
            connection: 1,
        })
    };
    let mut h = Harness::new("account-sentinel");
    let mut printed = Vec::new();
    let sign_in = |h: &mut Harness| {
        h.op(AccountOp::SignIn);
        let attempt = h.listening.last().expect("listening").0;
        h.feed(Input::Listened {
            attempt,
            result: Listened::Authorized(authorization()),
        });
    };
    sign_in(&mut h);
    printed.push(format!("{:?} {:?}", h.state, h.sent));
    h.reply(Err(ExchangeError::Response { status: 503 }));
    sign_in(&mut h);
    h.hold = true;
    h.reply(issued());
    printed.push(format!("{:?} {:?}", h.state, h.held));
    h.store.arm(0, Fault::Refuse);
    h.release();
    h.op(AccountOp::CancelSignIn);
    assert_eq!(h.revocations().len(), 1, "the refused set was listed");
    sign_in(&mut h);
    h.reply(issued());
    assert!(matches!(
        h.view(),
        AccountView::SignedIn { email: Some(_), .. }
    ));
    printed.push(format!("{:?} {:?} {:?}", h.state, h.views, h.errors));
    drop(guard);
    let logs = captured.text();
    assert!(
        logs.contains("a token request's outcome is unknown")
            && logs.contains("a verified sign-in is held for another save"),
        "the capture saw the account's warnings: {logs}"
    );
    for text in printed.iter().chain([&logs]) {
        assert!(!text.contains("SENTINEL"), "{text}");
    }
}

/// M4: a refused registration ends the attempt; the error offers a new sign-in, since Retry
/// would have nothing to rerun.
#[test]
fn a_refused_registration_ends_the_attempt_and_offers_a_new_sign_in() {
    let mut h = Harness::new("account-register-refused");
    h.op(AccountOp::SignIn);
    h.hold = true;
    h.callback("client-issued");
    h.store.arm(0, Fault::Refuse);
    h.release();
    assert!(h.sent.is_empty());
    assert_eq!(
        h.errors,
        [user_error(
            ErrorKey::CredentialsUnavailable,
            &[Action::SignIn]
        )]
    );
    assert_eq!(h.view(), &signed_out());
    assert!(h.account().is_none());
}

/// L1: a replaced attempt's registration landing first doesn't stop the replacement saving its
/// own, so the replacement's client survives its failed exchange.
#[test]
fn a_replacement_saves_its_own_registration_after_a_replaced_one_lands() {
    let mut h = Harness::new("account-register-race");
    h.op(AccountOp::SignIn);
    h.hold = true;
    h.callback("client-a");
    h.op(AccountOp::SignIn);
    h.release();
    assert_eq!(
        h.account().expect("a's registration").client_id.as_str(),
        "client-a"
    );
    h.callback("client-b");
    h.reply(Err(ExchangeError::OAuth {
        status: 400,
        code: OAuthCode::InvalidGrant,
    }));
    assert_eq!(
        h.account().expect("b's registration").client_id.as_str(),
        "client-b"
    );
}

/// L2: the commit reports the notice cleared only when its marker write landed.
#[test]
fn the_notice_is_reported_cleared_only_when_its_marker_write_lands() {
    const NOTICE: &str = r#"{"v":1,"sign_out":null,"sweep":false,"request_pending":false,"outcome_unknown":false,"notice":true}"#;
    let commit = |refuse_last: bool| {
        let scratch = Scratch::new("account-notice");
        let service = format!("test-account-{:016x}", getrandom::u64().expect("random"));
        let store = FakeStore::default();
        store.0.lock().expect("fake lock").values.insert(
            format!("{service}/signout"),
            Secret::new(Zeroizing::new(NOTICE.as_bytes().to_vec())),
        );
        let worker = |store: FakeStore, lock: std::path::PathBuf| {
            let name = service.clone();
            let mut worker = Worker::new(
                Box::new(move || Vault::with_service(store.clone(), &name, &lock)),
                Arc::new(FixedClock(NOW)),
            );
            assert!(matches!(
                worker.run(Job::Load { patient: false }),
                Done::Loaded(Ok(_))
            ));
            worker
        };
        let account = saved_account("subject-1", "refresh-new");
        let dry = store.fork();
        let mut rehearsal = worker(dry.clone(), scratch.db().with_file_name("dry.lock"));
        dry.arm(usize::MAX, Fault::Refuse);
        rehearsal.run(Job::Commit(account.clone()));
        let last = dry.0.lock().expect("fake lock").mutations.len() - 1;
        let mut real = worker(store.clone(), scratch.db().with_file_name(SESSION_LOCK));
        store.arm(if refuse_last { last } else { usize::MAX }, Fault::Refuse);
        let Done::Committed {
            result,
            notice_cleared,
            ..
        } = real.run(Job::Commit(account))
        else {
            panic!("a commit answers with Committed");
        };
        committed(&result);
        let state = store.0.lock().expect("fake lock");
        let marker = state
            .values
            .get(&format!("{service}/signout"))
            .map(|bytes| String::from_utf8(bytes.expose().to_vec()).expect("JSON"));
        (notice_cleared, marker)
    };
    assert_eq!(commit(false), (true, None));
    assert_eq!(commit(true), (false, Some(NOTICE.to_owned())));
}

#[test]
fn an_unreadable_host_refuses_sign_in_without_replacing_it() {
    let h = Harness::seeded("account-host", |vault| {
        committed(&vault.save_account(&saved_account("subject-1", "refresh-old")));
    });
    let host = format!("{}/host", h.service);
    let damaged = Secret::new(Zeroizing::new(b"\"not-a-host\"".to_vec()));
    h.store
        .0
        .lock()
        .expect("fake lock")
        .values
        .insert(host.clone(), damaged.clone());
    let mut h = h.restart();
    assert_eq!(
        h.view(),
        &signed_in(PlanState::Enabled, None),
        "the account still loads"
    );
    h.op(AccountOp::SignIn);
    assert!(h.listening.is_empty());
    assert_eq!(
        h.errors,
        [user_error(
            ErrorKey::CredentialsUnavailable,
            &[Action::Retry]
        )]
    );
    h.op(AccountOp::Retry);
    let state = h.store.0.lock().expect("fake lock");
    assert_eq!(
        state.values.get(&host).map(|v| v.expose().to_vec()),
        Some(damaged.expose().to_vec())
    );
}

#[test]
fn a_pending_sign_out_fences_the_account_and_starts_nothing() {
    let h = Harness::seeded("account-fenced", |vault| {
        committed(&vault.save_account(&saved_account("subject-1", "refresh-old")));
    });
    h.store.0.lock().expect("fake lock").values.insert(
        format!("{}/signout", h.service),
        Secret::new(Zeroizing::new(br#"{"v":2}"#.to_vec())),
    );
    let mut h = h.restart();
    let fenced = AccountView::SigningOut {
        cleanup: Cleanup::Running,
        revocation_unconfirmed: true,
    };
    assert_eq!(h.view(), &fenced);
    let jobs = h.markers.len();
    for op in [
        AccountOp::SignIn,
        AccountOp::EnablePlan,
        AccountOp::CancelSignIn,
        AccountOp::SignOut,
        AccountOp::Retry,
    ] {
        h.op(op);
        assert_eq!(h.view(), &fenced);
    }
    assert!(h.listening.is_empty());
    assert_eq!(h.markers.len(), jobs, "no op started a credential job");
    assert_eq!(h.revoking.len(), 1, "only the resumed sign-out's own run");
}

/// CAPS-117 M2: a patient load tries a busy session lock exactly five times in all.
#[test]
fn a_patient_load_tries_a_busy_lock_five_times_in_all() {
    for (busy, opened) in [(4, true), (5, false)] {
        let scratch = Scratch::new("account-busy");
        let lock = scratch.db().with_file_name(SESSION_LOCK);
        let store = FakeStore::default();
        let opens = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&opens);
        let mut worker = Worker::new(
            Box::new(move || {
                if counted.fetch_add(1, Ordering::SeqCst) < busy {
                    return Err(VaultError::Busy);
                }
                Vault::with_service(store.clone(), "test-account-busy", &lock)
            }),
            Arc::new(FixedClock(NOW)),
        );
        let done = worker.run(Job::Load { patient: true });
        assert_eq!(matches!(done, Done::Loaded(Ok(_))), opened, "{busy}");
        assert_eq!(
            matches!(done, Done::Loaded(Err(VaultError::Busy))),
            !opened,
            "{busy}"
        );
        assert_eq!(opens.load(Ordering::SeqCst), 5, "{busy}");
    }
}

#[test]
fn a_lasting_busy_lock_is_retried_on_each_op() {
    let scratch = Scratch::new("account-busy-lasting");
    let lock = scratch.db().with_file_name(SESSION_LOCK);
    let held = SessionLock::try_acquire(&lock).expect("lock");
    let mut h = Harness::boot(
        scratch,
        "test-account-busy".to_owned(),
        FakeStore::default(),
        Wall::new(),
    );
    assert_eq!(h.views, [AccountView::Busy]);
    h.op(AccountOp::SignIn);
    assert_eq!(h.view(), &AccountView::Busy);
    assert!(h.listening.is_empty());
    drop(held);
    h.feed(Input::Connected);
    assert_eq!(h.view(), &signed_out());
}

#[test]
fn enabling_the_plan_without_the_plan_scope_says_so_and_saves_it_declined() {
    let mut h = Harness::new("account-plan");
    h.op(AccountOp::EnablePlan);
    assert_eq!(h.listening[0].1, Intent::EnablePlan);
    h.callback("client-issued");
    h.reply(received(
        Some("refresh-a"),
        "subject-1",
        "openid offline_access",
    ));
    assert_eq!(h.view(), &signed_in(PlanState::Declined, None));
    assert_eq!(
        h.errors,
        [user_error(ErrorKey::PlanNotEnabled, &[Action::Enable])]
    );
    assert!(!h.account().expect("account").plan_enabled);
}

/// Pick 2: an account whose plan the catalog refused shows Unavailable, and its sign-out erases
/// the registration; one in good standing keeps it.
#[test]
fn a_plan_the_catalog_refused_is_unavailable_and_its_sign_out_erases_the_registration() {
    for refused in [false, true] {
        let mut h = Harness::seeded("sign-out-unavailable", |vault| {
            committed(&vault.save_account(&saved_account("subject-1", "refresh-old")));
        });
        if refused {
            h.feed(Input::PlanUnavailable);
        }
        let plan = if refused {
            PlanState::Unavailable
        } else {
            PlanState::Enabled
        };
        assert_eq!(h.view(), &signed_in(plan, None), "{refused}");
        h.op(AccountOp::SignOut);
        let mut answered = 0;
        while let Some(run) = h.revoking.get(answered).cloned() {
            answered += 1;
            h.feed(Input::Revoked(run));
        }
        assert_eq!(h.view(), &signed_out(), "{refused}");
        let kept = h.account().map(|account| account.client_id.to_string());
        assert_eq!(
            kept,
            (!refused).then(|| "client-saved".to_owned()),
            "{refused}"
        );
    }
}
