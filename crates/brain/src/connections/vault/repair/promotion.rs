use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc;

use kumochi_protocol::Secret;
use zeroize::Zeroizing;

use super::exchange::decode_escrow;
use super::{DISPATCHED, RETIRED, RepairAttempt};
use crate::connections::authority::{AuthorityError, Generation, Kind, repair::ExchangeStep};
use crate::connections::clock::Clock;
use crate::connections::error::VaultError;
use crate::connections::http_client::Http;
use crate::connections::oauth::{
    exchange::TokenSet,
    id_token::{permits_plan, verify},
};
use crate::connections::records::{Account, MAX_RECORD_BYTES, Record as _, RevocationList, Tokens};
use crate::connections::vault::{Vault, encode, generation};
use crate::platform::credential::CredentialStore;
use crate::store::{Boundary, StoreError};

#[derive(Debug)]
pub(in crate::connections) struct RepairProof {
    owner: Arc<AtomicU8>,
    client: Secret<Zeroizing<String>>,
    nonce: Secret<Zeroizing<String>>,
    tokens: TokenSet,
    received_at: Option<u64>,
}

#[derive(Debug)]
pub(in crate::connections) struct VerifiedReplacement {
    owner: Arc<AtomicU8>,
    account: Secret<Account>,
    window: Range<u64>,
}

/// The vault's binding, made once at construction, to the profile's one history writer: the
/// store thread's store, which ordinary startup opens and purge guards. The writer serves each
/// request as one of its operations, so a purge sees the cutoff write. Unbound, every request is
/// refused.
pub(crate) struct HistoryLink(Option<Box<dyn Fn(CutoffRequest) -> bool + Send>>);

impl HistoryLink {
    /// `send` queues a request for the writer; false when the writer has gone.
    pub(crate) fn new(send: impl Fn(CutoffRequest) -> bool + Send + 'static) -> Self {
        Self(Some(Box::new(send)))
    }

    pub(crate) fn unbound() -> Self {
        Self(None)
    }

    /// The committed cutoff for `tag`, read again by the writer.
    pub(crate) fn check(&self, tag: u64) -> Result<Boundary, StoreError> {
        let send = self.0.as_ref().ok_or(StoreError::RecoveryContext)?;
        let (reply, answer) = mpsc::sync_channel(1);
        if !send(CutoffRequest(Ask::Check(tag, reply))) {
            return Err(StoreError::RecoveryContext);
        }
        answer.recv().unwrap_or(Err(StoreError::RecoveryContext))
    }
}

/// One request for the history writer. Only a bound replacement can ask for a write, and the
/// write's raw form is reachable only through [`CutoffRequest::serve`].
pub(crate) struct CutoffRequest(Ask);

type Answer<T> = mpsc::SyncSender<Result<T, CutoffRefused>>;

enum Ask {
    Establish(PendingReplacement, Answer<CutoffReceipt>),
    Check(u64, mpsc::SyncSender<Result<Boundary, StoreError>>),
}

/// What the writer does for one request: write `tag`'s cutoff, setting the flag once its commit
/// is attempted, or read it again.
pub(crate) enum Work<'a> {
    Write(u64, &'a mut bool),
    Read(u64),
}

impl CutoffRequest {
    pub(crate) fn establishes(&self) -> bool {
        matches!(self.0, Ask::Establish(..))
    }

    /// Runs the request's work and answers it. A write's binding comes back only when its commit
    /// was never attempted.
    pub(crate) fn serve(self, run: impl FnOnce(Work<'_>) -> Result<Boundary, StoreError>) {
        match self.0 {
            Ask::Establish(pending, reply) => {
                let mut attempted = false;
                let answer = match run(Work::Write(pending.tag, &mut attempted)) {
                    Ok(boundary) => Ok(CutoffReceipt {
                        successor: pending.successor,
                        boundary,
                    }),
                    Err(error) => Err(CutoffRefused {
                        pending: (!attempted).then_some(pending),
                        error,
                    }),
                };
                reply.send(answer).ok();
            }
            Ask::Check(tag, reply) => {
                reply.send(run(Work::Read(tag))).ok();
            }
        }
    }
}

/// A refused Establish. The binding comes back only when no commit was attempted; once one was,
/// or when the writer's answer is lost, it is spent and no second cutoff can be minted.
#[derive(Debug)]
pub(crate) struct CutoffRefused {
    pub(crate) pending: Option<PendingReplacement>,
    pub(crate) error: StoreError,
}

/// What one committed Establish wrote; only a matching fresh Check by the history writer
/// confirms it.
#[derive(Debug)]
#[must_use]
pub(crate) struct CutoffReceipt {
    successor: Generation,
    boundary: Boundary,
}

/// The one-shot proof of an exact locked binding. It moves into the writer's Establish request,
/// so it can mint at most one cutoff.
#[derive(Debug)]
#[must_use]
pub(crate) struct PendingReplacement {
    successor: Generation,
    tag: u64,
    issuer: Arc<()>,
}

impl PendingReplacement {
    pub(crate) fn binding(&self) -> (Generation, u64) {
        (self.successor, self.tag)
    }
}

impl RepairProof {
    pub(super) fn new(
        owner: Arc<AtomicU8>,
        client: Secret<Zeroizing<String>>,
        nonce: Secret<Zeroizing<String>>,
        tokens: TokenSet,
        received_at: Option<u64>,
    ) -> Self {
        Self {
            owner,
            client,
            nonce,
            tokens,
            received_at,
        }
    }

    pub(in crate::connections) async fn verify(
        self,
        http: &impl Http,
        clock: &dyn Clock,
    ) -> Option<VerifiedReplacement> {
        if self.owner.load(Ordering::Acquire) != DISPATCHED
            || !permits_plan(self.tokens.scope.expose())
        {
            return None;
        }
        let received_at = self.received_at.filter(|now| *now != 0)?;
        let access_expiry = received_at.checked_add(self.tokens.expires_in)?;
        let id = self.tokens.id_token?;
        let refresh = self.tokens.refresh_token?;
        let identity = verify(http, &id, self.client.expose(), self.nonce.expose(), clock)
            .await
            .ok()?;
        let expires_at = access_expiry.min(identity.expires_at);
        let window = received_at..expires_at;
        if !clock
            .unix_seconds()
            .is_some_and(|now| window.contains(&now))
            || self.owner.load(Ordering::Acquire) != DISPATCHED
        {
            return None;
        }
        let tag = getrandom::u64().ok()? & ((1_u64 << 63) - 1);
        if tag == 0 {
            return None;
        }
        let account = Account {
            client_id: self.client.expose().clone(),
            subject: Some(identity.subject.expose().clone()),
            email: identity.email.map(|email| email.expose().clone()),
            plan_enabled: true,
            tokens: Some(Tokens {
                access_token: self.tokens.access_token.expose().clone(),
                refresh_token: refresh.expose().clone(),
                id_token: Some(id.expose().clone()),
                scope: self.tokens.scope.expose().clone(),
                expires_at,
                pending_identity_validation: false,
                refresh_sent: false,
                connection: tag,
            }),
        };
        account.validate().ok()?;
        Some(VerifiedReplacement {
            owner: self.owner,
            account: Secret::new(account),
            window,
        })
    }
}

impl<S: CredentialStore> Vault<S> {
    pub(in crate::connections) fn bind_repair_replacement(
        &mut self,
        attempt: &RepairAttempt,
        verified: VerifiedReplacement,
        clock: &dyn Clock,
    ) -> Result<PendingReplacement, VaultError> {
        let VerifiedReplacement {
            owner,
            account,
            window,
        } = verified;
        let current_time = || {
            clock
                .unix_seconds()
                .is_some_and(|now| window.contains(&now))
        };
        if !self.owns_repair(attempt)
            || !Arc::ptr_eq(&owner, &attempt.lease.0)
            || owner.load(Ordering::Acquire) != DISPATCHED
            || !current_time()
        {
            return Err(VaultError::Fenced);
        }
        let account = account.expose();
        let tokens = account.tokens.as_ref().ok_or(VaultError::Fenced)?;
        let tag = tokens.connection;
        let host = self
            .authority
            .current(Kind::Host)?
            .active
            .ok_or(VaultError::Fenced)?;
        let revocations = self.authority.current(Kind::Revocations)?.active;
        let host_bytes = encode(attempt.host.expose(), 128)?;
        let empty = encode(&RevocationList::default(), MAX_RECORD_BYTES)?;
        let bytes = encode(account, MAX_RECORD_BYTES)?;
        let plan = self.authority.stage_repair_credential(
            &attempt.owner,
            ExchangeStep::Replace,
            bytes.expose().len(),
        )?;
        let (store, service) = (&self.store, self.service.as_str());
        let verified = generation::write(store, service, plan, &bytes)?;
        let generation = self.authority.bind_repair_replacement(
            &attempt.owner,
            verified,
            tag,
            |old, escrow, plan| {
                let recorded = generation::read_active(store, service, &escrow)?;
                let (client, handle) =
                    decode_escrow(recorded.expose()).map_err(|_| AuthorityError::Corrupt)?;
                if client != account.client_id.as_str()
                    || handle != tokens.refresh_token.as_str()
                    || generation::read_active(store, service, &host)?.expose()
                        != host_bytes.expose()
                    || !generation::has_missing_chunk(store, service, &old)?
                {
                    return Err(AuthorityError::Conflict);
                }
                if let Some(active) = revocations
                    && generation::read_active(store, service, &active)?.expose() != empty.expose()
                {
                    return Err(AuthorityError::Conflict);
                }
                if generation::read_staged_once(store, service, plan)?.expose() != bytes.expose()
                    || !current_time()
                    || owner
                        .compare_exchange(DISPATCHED, RETIRED, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                {
                    return Err(AuthorityError::Conflict);
                }
                Ok(())
            },
        )?;
        Ok(PendingReplacement {
            successor: generation,
            tag,
            issuer: Arc::clone(&self.incarnation),
        })
    }

    /// Has this vault's history writer write `pending`'s one cutoff. A token another vault issued,
    /// or an unbound vault, is refused before anything is queued and handed back.
    pub(crate) fn establish_history_cutoff(
        &self,
        pending: PendingReplacement,
    ) -> Result<CutoffReceipt, CutoffRefused> {
        let send = match &self.history.0 {
            Some(send) if Arc::ptr_eq(&pending.issuer, &self.incarnation) => send,
            _ => {
                return Err(CutoffRefused {
                    pending: Some(pending),
                    error: StoreError::RecoveryContext,
                });
            }
        };
        let (reply, answer) = mpsc::sync_channel(1);
        let lost = || CutoffRefused {
            pending: None,
            error: StoreError::RecoveryContext,
        };
        if !send(CutoffRequest(Ask::Establish(pending, reply))) {
            return Err(lost());
        }
        answer.recv().unwrap_or_else(|_| Err(lost()))
    }

    /// Marks the bound replacement's context established only when the history writer's fresh
    /// Check reads exactly the row its receipt wrote. The receipt is spent either way.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "every call spends the receipt"
    )]
    pub(crate) fn establish_repair_context(
        &mut self,
        receipt: CutoffReceipt,
    ) -> Result<(), VaultError> {
        let CutoffReceipt {
            successor,
            boundary,
        } = receipt;
        if self.history.check(boundary.tag()).ok() != Some(boundary) {
            return Err(VaultError::Fenced);
        }
        Ok(self
            .authority
            .establish_repair_context(successor, boundary.tag(), boundary.bounds())?)
    }

    /// The established replacement's cutoff, once the history writer reads its row again and
    /// it matches; every read for the replacement keeps to it. A missing, changed or unreadable
    /// row leaves the replacement locked.
    pub(crate) fn established_repair_context(&self) -> Result<Boundary, VaultError> {
        let (tag, bounds) = self.authority.established_repair_context()?;
        match self.history.check(tag) {
            Ok(boundary) if boundary.bounds() == bounds => Ok(boundary),
            _ => Err(VaultError::Fenced),
        }
    }
}
