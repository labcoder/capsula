//! Claude Code's connection: what discovery found, through the one lane that starts `claude`.
//! A1 owns the Anthropic key's half of the view; every Connections frame is put together from
//! both at the moment it's sent.

use std::sync::Arc;
use std::time::Instant;

use kumochi_protocol::constants::CLAUDE_QUESTIONS_ENV;
use kumochi_protocol::message::{
    Action, AuditFailed, ClaudeCodeView, ClaudeVersion, ConnectionKind, ConnectionOp,
    ConnectionsView, ControlToShell, DataToShell, Effort, ErrorKey, ModelChoice, ModelInfo,
    ModelName, ModelSlug, ThreadId, Unsupported, UserError,
};
use tokio::sync::mpsc;

use super::{Brain, Ended, Full, Link};
use crate::bridge::LaunchId;
use crate::connections::model::budget::Budget;
use crate::context::ConnectionTag;
use crate::coordinator::{Availability, Input, Ready, StoreOp, model_unavailable};
use crate::engines::claude_code::install::FindError;
use crate::engines::claude_code::lane::{Done, Failure, Lane, Outcome, Work};
use crate::engines::claude_code::launch::{
    self, Attempt, ClaudeQuestion, Clock, LaunchSnapshot, Note, Offer, Refresh, Report, WARM,
};
use crate::engines::claude_code::policy::{self, Reviewed};
use crate::engines::claude_code::supervisor::{
    self, Control, Launch, LaunchRefused, MAX_LAUNCH, Question,
};
use crate::engines::{EngineFailure, RunRequest};

/// Where Claude Code's sign-in may be kept, which owner call 1 decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SignInStore {
    /// Not decided: Connect does nothing and a signed-out install shows as unsupported.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the off position, kept under test")
    )]
    Undecided,
    /// Claude Code's own file in Kumochi's owner-only folder (option a). Only it lets Connect,
    /// Cancel and Disconnect do anything.
    FileWithDacl,
}

/// What the shipped brain allows: the owner chose option a for call 1 (2026-11-05).
pub(crate) const SIGN_IN: SignInStore = SignInStore::FileWithDacl;

/// The connection's state; the lane does the work.
#[derive(Debug)]
pub(crate) struct Claude {
    lane: Option<Lane>,
    sign_in: SignInStore,
    /// False under `[claude_code] enabled = false`, where only a marked deletion runs.
    finds: bool,
    generation: u64,
    view: ClaudeCodeView,
    /// The job of this generation that's out, if one is.
    out: Option<Work>,
    /// A Find was asked for while it was.
    again: bool,
    /// A Disconnect is still to finish.
    cleaning: bool,
    /// The generation a Cancel started: its Status decides whether Kumochi's folder goes.
    cancelled: Option<u64>,
    /// The launches admitted, until each one's teardown is over and its report, if one is due,
    /// was heard.
    launches: Vec<Held>,
    /// The version whose audit failed in this brain, until a different reviewed version is found
    /// once its marker is written. The marker itself stays.
    quarantine: Option<Quarantine>,
    /// The warm attempt, waiting for its question.
    warm: Option<Attempt>,
    /// What attempts tell the core.
    notes: (mpsc::UnboundedSender<Note>, mpsc::UnboundedReceiver<Note>),
    clock: Clock,
    /// The questions seeds may still take.
    budget: Arc<Budget>,
    /// The tag runs on Claude Code are bound to, while a launch would be admitted.
    epoch: Option<Epoch>,
    /// The last epoch's tag.
    epochs: u64,
    /// The models runs may name now: the reviewed row's while a launch would be admitted.
    models: Vec<ModelInfo>,
    /// The list a shell hasn't heard yet.
    owed: Option<Vec<ModelInfo>>,
}

/// Runs bound under one epoch go on through a Find; a sign-out, a Disconnect, an audit failure,
/// a stuck lane or another version ends it, and the next admitted state draws a new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Epoch {
    tag: u64,
    version: ClaudeVersion,
}

/// One admitted launch, as the core tracks it.
#[derive(Debug)]
struct Held {
    thread: ThreadId,
    id: LaunchId,
    control: Arc<Control>,
    /// The generation and version it was admitted under.
    generation: u64,
    version: ClaudeVersion,
    /// An attempt's one report is still to come.
    due: bool,
    /// A replacement revoked it: its report changes nothing.
    replaced: bool,
}

/// What woke the connection.
#[derive(Debug)]
pub(crate) enum Wake {
    Lane(Done),
    Note(Note),
    /// The warm attempt's window may have closed.
    Expired,
}

/// A version this brain saw fail its audit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Quarantine {
    version: ClaudeVersion,
    reason: AuditFailed,
    /// Its marker was written.
    durable: bool,
}

impl Default for Claude {
    /// No lane: nothing is ever found.
    fn default() -> Self {
        Self::new(None, SIGN_IN, ClaudeCodeView::NotFound)
    }
}

impl Claude {
    pub(super) fn new(lane: Option<Lane>, sign_in: SignInStore, view: ClaudeCodeView) -> Self {
        Self {
            lane,
            sign_in,
            finds: true,
            generation: 0,
            view,
            out: None,
            again: false,
            cleaning: false,
            cancelled: None,
            launches: Vec::new(),
            quarantine: None,
            warm: None,
            notes: mpsc::unbounded_channel(),
            clock: Clock::default(),
            budget: Arc::new(Budget::from_var(CLAUDE_QUESTIONS_ENV)),
            epoch: None,
            epochs: 0,
            models: Vec::new(),
            owed: None,
        }
    }

    /// `[claude_code] enabled = false`: nothing is found or started, and `lane` only finishes a
    /// deletion an earlier Disconnect marked.
    pub(crate) fn turned_off(lane: Option<Lane>) -> Self {
        let mut claude = Self::new(lane, SIGN_IN, ClaudeCodeView::TurnedOff);
        claude.finds = false;
        claude.find();
        claude
    }

    /// Discovery through `lane`, starting now.
    pub(crate) fn start(lane: Lane, sign_in: SignInStore) -> Self {
        let mut claude = Self::new(Some(lane), sign_in, ClaudeCodeView::NotFound);
        claude.find();
        claude
    }

    pub(crate) fn view(&self) -> ClaudeCodeView {
        self.view
    }

    /// Starts `work` under a new generation, which ends whatever an older one still runs; false
    /// when it isn't out.
    fn begin(&mut self, work: Work) -> bool {
        let Some(lane) = &self.lane else {
            return false;
        };
        self.generation += 1;
        lane.fence(self.generation);
        self.again = false;
        self.submit(work)
    }

    /// Queues `work` under this generation. When the lane can't take it, nothing is out and the
    /// view says so, since no result will come to settle it.
    fn submit(&mut self, work: Work) -> bool {
        let admitted = self
            .lane
            .as_ref()
            .is_some_and(|lane| lane.submit(self.generation, work));
        self.out = admitted.then_some(work);
        if !admitted {
            self.view = ClaudeCodeView::Unavailable;
            self.revoke_all();
        }
        admitted
    }

    /// Finds the install again, or retries a deletion still to finish; after the job that's out,
    /// if one is.
    fn find(&mut self) {
        // shortcut: a pending deletion is retried here and at each start, not on a timer; add one
        // if locked files turn out to outlive a session.
        if self.out.is_some() {
            self.again = true;
            return;
        }
        match (self.finds, self.cleaning) {
            (false, _) => {
                self.begin(Work::Cleanup);
            }
            (true, true) => {
                self.begin(Work::Disconnect { logout: false });
            }
            (true, false) => match self.quarantine {
                Some(Quarantine {
                    version,
                    durable: false,
                    ..
                }) => {
                    self.begin(Work::Quarantine(version));
                }
                _ => {
                    if self.begin(Work::Version)
                        && !matches!(self.view, ClaudeCodeView::Ready { .. })
                    {
                        self.view = ClaudeCodeView::Finding;
                    }
                }
            },
        }
    }

    /// Refuses `version` from now on for `reason`: every launch ends, and the lane writes its
    /// marker, which survives Disconnect and restarts. Until that write succeeds, every version
    /// stays refused. The first failure's reason stays.
    pub(crate) fn audit_failed(&mut self, version: ClaudeVersion, reason: AuditFailed) {
        if self.quarantine.is_none_or(|held| held.version != version) {
            self.quarantine = Some(Quarantine {
                version,
                reason,
                durable: false,
            });
        }
        self.revoke_all();
        if let Some(held) = self.quarantine {
            self.view = ClaudeCodeView::AuditFailed(held.reason);
        }
        self.find();
    }

    /// One of Claude Code's ops; every one is answered with the view.
    pub(crate) fn op(&mut self, op: ConnectionOp) {
        let signs_in = self.sign_in == SignInStore::FileWithDacl;
        match op {
            ConnectionOp::FindClaudeCode => self.find(),
            ConnectionOp::ConnectClaudeCode
                if signs_in
                    && self.out.is_none()
                    && matches!(self.view, ClaudeCodeView::SignedOut { .. }) =>
            {
                if self.begin(Work::Login) {
                    self.view = ClaudeCodeView::SigningIn;
                }
            }
            // The new generation ends the sign-in; the Status after it shows where it got. Only
            // the sign-in itself is cancelled, so a repeated Cancel queues nothing more.
            ConnectionOp::CancelClaudeSignIn
                if signs_in
                    && self.view == ClaudeCodeView::SigningIn
                    && self.out == Some(Work::Login) =>
            {
                if self.begin(Work::Version) {
                    self.cancelled = Some(self.generation);
                }
            }
            // A Disconnect the lane can't take leaves nothing marked, so it may be asked again.
            ConnectionOp::DisconnectClaudeCode if signs_in && self.finds && !self.cleaning => {
                // Before the Disconnect is queued, so the lane ends every launch before it signs
                // out.
                self.revoke_all();
                self.cleaning = self.begin(Work::Disconnect { logout: true });
                if self.cleaning {
                    self.view = ClaudeCodeView::CleanupPending;
                }
            }
            _ => {}
        }
    }

    /// A lane result; true when the view may have changed.
    pub(crate) fn done(&mut self, Done { generation, result }: Done) -> bool {
        if generation != self.generation || self.out.is_none() {
            return false;
        }
        if let Some(held) = &mut self.quarantine
            && self.out == Some(Work::Quarantine(held.version))
            && !matches!(
                result,
                Err(Failure::Quarantine | Failure::Refused | Failure::Busy)
            )
        {
            held.durable = true;
        }
        // A reviewed version found past the quarantine check; the marker keeps the refused one
        // refused.
        if let Ok(Outcome::Version(found)) = result
            && self
                .quarantine
                .is_some_and(|held| held.durable && held.version != found)
        {
            self.quarantine = None;
        }
        // The Version and Status after a Cancel: unless the Status is signed in, or the lane
        // couldn't run the call (Busy, Refused), the sign-in may have left Claude Code's file in
        // Kumochi's folder, and only a Disconnect removes it.
        let leftover = self.cancelled == Some(generation)
            && matches!(self.out, Some(Work::Version | Work::Status))
            && !matches!(
                result,
                Ok(Outcome::Status {
                    signed_in: true,
                    ..
                }) | Err(Failure::Busy | Failure::Refused)
            );
        match result {
            Ok(Outcome::Version(_) | Outcome::LoginEnded) => {
                if self.submit(Work::Status) {
                    return false;
                }
            }
            Ok(Outcome::Status { .. }) if let Some(held) = self.quarantine => {
                self.view = ClaudeCodeView::AuditFailed(held.reason);
            }
            Ok(Outcome::Status { version, signed_in }) => {
                self.view = match (signed_in, self.sign_in) {
                    (true, _) => ClaudeCodeView::Ready {
                        version,
                        // The audit's to report, when C5b runs it.
                        analytics_on: false,
                    },
                    (false, SignInStore::Undecided) => {
                        ClaudeCodeView::Unsupported(Unsupported::SignInStore)
                    }
                    (false, SignInStore::FileWithDacl) => ClaudeCodeView::SignedOut { version },
                };
            }
            Ok(Outcome::Removed) => {
                self.cleaning = false;
                if self.finds {
                    self.again = true;
                } else {
                    self.view = ClaudeCodeView::TurnedOff;
                }
            }
            Err(Failure::Quarantined(version)) => {
                self.view = ClaudeCodeView::AuditFailed(
                    self.quarantine
                        .filter(|held| held.version == version)
                        .map_or(AuditFailed::Version, |held| held.reason),
                );
            }
            Err(failure) => {
                self.view = match self.quarantine {
                    Some(held) if !held.durable => ClaudeCodeView::AuditFailed(held.reason),
                    _ => failed(failure),
                };
            }
        }
        self.out = None;
        if leftover {
            self.cleaning = self.begin(Work::Disconnect { logout: false });
            if self.cleaning {
                self.view = ClaudeCodeView::CleanupPending;
            }
        }
        if std::mem::take(&mut self.again) {
            self.find();
        }
        // A settled view that isn't Ready ends every launch; one still being found doesn't.
        if self.out.is_none() && !matches!(self.view, ClaudeCodeView::Ready { .. }) {
            self.revoke_all();
        }
        true
    }

    /// Queues a question launch behind whatever the lane is doing. Its deadline is fixed now.
    ///
    /// # Errors
    /// [`LaunchRefused::NotReady`] unless [`admitted`](Self::admitted); [`LaunchRefused::Lifetime`]
    /// over [`MAX_LAUNCH`]; [`LaunchRefused::Identity`] when no launch id can be drawn;
    /// [`LaunchRefused::Busy`] when the lane's queue is full. Nothing is queued then.
    pub(crate) fn launch(&mut self, question: Question) -> Result<Launch, LaunchRefused> {
        self.prune();
        let (version, _) = self.admitted().ok_or(LaunchRefused::NotReady)?;
        let lane = self.lane.as_ref().ok_or(LaunchRefused::NotReady)?;
        if question.lifetime > MAX_LAUNCH {
            return Err(LaunchRefused::Lifetime);
        }
        let thread = question.thread;
        let (start, launch) = supervisor::admit(question)?;
        let control = start.control();
        if !lane.launch(start) {
            return Err(LaunchRefused::Busy);
        }
        self.launches.push(Held {
            thread,
            id: launch.id(),
            control,
            generation: self.generation,
            version,
            due: false,
            replaced: false,
        });
        Ok(launch)
    }

    /// The version and its reviewed row while a question launch would be admitted: Ready under
    /// the decided sign-in store, on, no Disconnect pending, no version quarantined, a lane, and
    /// a reviewed version. [`launch`](Self::launch) and [`availability`](Self::availability)
    /// both ask this, so a run is never bound to Claude Code while a launch would be refused.
    fn admitted(&self) -> Option<(ClaudeVersion, &'static Reviewed)> {
        let ClaudeCodeView::Ready { version, .. } = self.view else {
            return None;
        };
        let ready = self.sign_in == SignInStore::FileWithDacl
            && self.finds
            && !self.cleaning
            && self.quarantine.is_none()
            && self.lane.is_some();
        ready
            .then(|| policy::reviewed(version).ok())
            .flatten()
            .map(|row| (version, row))
    }

    /// Whether runs on Claude Code can be answered now: Ready under its epoch with the reviewed
    /// version's models exactly when a launch would be admitted, else `Finding` while it's being
    /// found (a typed turn waits for the next report), else the view's reason. The
    /// runtime asks after every op and wake, the only ways the view changes, so an epoch is
    /// never kept across a state that wasn't admitted.
    pub(crate) fn availability(&mut self) -> Availability {
        let Some((version, row)) = self.admitted() else {
            self.epoch = None;
            self.offer(Vec::new());
            if self.view == ClaudeCodeView::Finding {
                return Availability::Finding;
            }
            return Availability::Unavailable(self.reason());
        };
        let epoch = match self.epoch {
            Some(epoch) if epoch.version == version => epoch,
            _ => {
                self.epochs += 1;
                Epoch {
                    tag: self.epochs,
                    version,
                }
            }
        };
        self.epoch = Some(epoch);
        let models: Vec<ModelInfo> = row
            .models
            .iter()
            .map(|&(slug, name)| ModelInfo {
                slug: ModelSlug::new(slug).expect("a reviewed alias is a valid slug"),
                display_name: ModelName::new(name).expect("a reviewed name is valid"),
            })
            .collect();
        self.offer(models.clone());
        Availability::Ready(Ready {
            connection: ConnectionTag(epoch.tag),
            models,
            #[cfg(any(test, debug_assertions))]
            trusted_draw: false,
        })
    }

    /// Records the models runs may name now; a change is owed to the shell.
    fn offer(&mut self, models: Vec<ModelInfo>) {
        if self.models != models {
            self.owed = Some(models.clone());
        }
        self.models = models;
    }

    /// A shell connected: it hears the models on offer, if there are any, and nothing older.
    pub(super) fn connected(&mut self) {
        self.owed = (!self.models.is_empty()).then(|| self.models.clone());
    }

    fn models_unsent(&mut self) -> Option<DataToShell> {
        self.owed.take().map(|models| DataToShell::Models {
            connection: ConnectionKind::ClaudeCode,
            models,
        })
    }

    /// Why runs on Claude Code can't be answered now, with Open Settings: the view's own key, or
    /// `ModelUnavailable` while it's being found or is turned off.
    fn reason(&self) -> UserError {
        let key = match self.view {
            ClaudeCodeView::NotFound => ErrorKey::ClaudeCodeNotFound,
            ClaudeCodeView::SignedOut { .. } | ClaudeCodeView::SigningIn => {
                ErrorKey::ClaudeCodeSignedOut
            }
            ClaudeCodeView::PolicyFailed(_) => ErrorKey::ClaudeCodePolicy,
            ClaudeCodeView::AuditFailed(_) => ErrorKey::ClaudeCodeAuditFailed,
            ClaudeCodeView::CleanupPending => ErrorKey::ClaudeCodeCleanupPending,
            ClaudeCodeView::Finding | ClaudeCodeView::TurnedOff => return model_unavailable(),
            ClaudeCodeView::Unsupported(_)
            | ClaudeCodeView::InUse
            | ClaudeCodeView::Unavailable
            | ClaudeCodeView::Ready { .. } => ErrorKey::ClaudeCodeUnsupported,
        };
        UserError::new(key, Vec::new(), vec![Action::OpenSettings])
            .expect("one action is within the limit")
    }

    /// The engine for one question on `request`'s Claude Code choice, offering no tools. The
    /// model is the reviewed row's own alias, so only a listed one reaches the command line
    /// whatever the request names, and the effort goes by a table to the CLI's word.
    ///
    /// # Errors
    /// [`LaunchRefused::NotReady`] unless a launch would be admitted and the choice is a Claude
    /// Code one whose model the row lists; otherwise as [`take`](Self::take).
    pub(crate) fn question(
        &mut self,
        request: &RunRequest,
    ) -> Result<ClaudeQuestion, LaunchRefused> {
        let (_, row) = self.admitted().ok_or(LaunchRefused::NotReady)?;
        let choice = request
            .request
            .choice
            .as_ref()
            .filter(|choice| choice.connection == ConnectionKind::ClaudeCode)
            .ok_or(LaunchRefused::NotReady)?;
        let model = row
            .models
            .iter()
            .map(|&(slug, _)| slug)
            .find(|&slug| slug == choice.model.as_str())
            .ok_or(LaunchRefused::NotReady)?;
        let effort = match choice.effort {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
            Effort::Max => "max",
        };
        let snapshot = self.snapshot(
            request.thread,
            model.to_owned(),
            Some(effort.to_owned()),
            Offer::Nothing,
        )?;
        self.take(&snapshot)
    }

    /// Claude Code's half of the brain's repair of `default_model`, while a launch would be
    /// admitted: the reviewed row's own default at `Low` when `current` is unset or a Claude Code
    /// alias the row doesn't list. A choice on another connection is never moved.
    pub(crate) fn repaired_default(&self, current: Option<&ModelChoice>) -> Option<ModelChoice> {
        let (_, row) = self.admitted()?;
        if current.is_some_and(|choice| {
            choice.connection != ConnectionKind::ClaudeCode
                || row
                    .models
                    .iter()
                    .any(|&(slug, _)| slug == choice.model.as_str())
        }) {
            return None;
        }
        Some(ModelChoice {
            model: ModelSlug::new(row.default_model).expect("a reviewed alias is a valid slug"),
            effort: Effort::Low,
            connection: ConnectionKind::ClaudeCode,
        })
    }

    /// What a run's round fails with when its question was refused: Claude Code's own reason
    /// while no launch would be admitted, else none. Never `Access(None)`, which reads as a
    /// `ChatGPT` sign-in.
    pub(crate) fn refused(&self, refused: LaunchRefused) -> EngineFailure {
        match refused {
            LaunchRefused::NotReady if self.admitted().is_none() => {
                EngineFailure::Claude(self.reason())
            }
            LaunchRefused::NotReady
            | LaunchRefused::Busy
            | LaunchRefused::Identity
            | LaunchRefused::Lifetime => EngineFailure::Provider,
        }
    }

    /// What a launch for `thread` would be fixed to now.
    ///
    /// # Errors
    /// [`LaunchRefused::NotReady`] unless Claude Code is Ready.
    pub(crate) fn snapshot(
        &self,
        thread: ThreadId,
        model: String,
        effort: Option<String>,
        offer: Offer,
    ) -> Result<LaunchSnapshot, LaunchRefused> {
        let ClaudeCodeView::Ready { version, .. } = self.view else {
            return Err(LaunchRefused::NotReady);
        };
        Ok(LaunchSnapshot {
            thread,
            model,
            effort,
            account: self.generation,
            offer,
            version,
        })
    }

    /// Starts a launch for `snapshot`'s question before it's asked. A repeat for the same
    /// snapshot keeps the warm one and its expiry; another snapshot replaces it.
    ///
    /// # Errors
    /// As [`launch`](Self::launch), and [`LaunchRefused::NotReady`] for a stale snapshot.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "C5b-2's engine selection is the first caller")
    )]
    pub(crate) fn warm(&mut self, snapshot: LaunchSnapshot) -> Result<LaunchId, LaunchRefused> {
        if let Some(warm) = &self.warm
            && *warm.snapshot() == snapshot
            && self.current(&snapshot)
            && self.live(warm.id())
        {
            return Ok(warm.id());
        }
        if let Some(warm) = self.warm.take() {
            self.replaced(warm.id());
        }
        let attempt = self.attempt(snapshot, Some(self.clock.now() + WARM))?;
        let id = attempt.id();
        self.warm = Some(attempt);
        Ok(id)
    }

    /// The question's engine: the warm attempt if its snapshot is equal and it hasn't expired,
    /// else a fresh one. The warm attempt goes either way.
    ///
    /// # Errors
    /// As [`warm`](Self::warm).
    pub(crate) fn take(
        &mut self,
        snapshot: &LaunchSnapshot,
    ) -> Result<ClaudeQuestion, LaunchRefused> {
        let now = self.clock.now();
        let attempt = match self.warm.take() {
            Some(warm)
                if warm.snapshot() == snapshot
                    && self.current(snapshot)
                    && self.live(warm.id())
                    && warm.expires().is_some_and(|expires| now < expires) =>
            {
                warm
            }
            stale => {
                if let Some(stale) = stale {
                    self.replaced(stale.id());
                }
                self.attempt(snapshot.clone(), None)?
            }
        };
        Ok(ClaudeQuestion::new(
            attempt,
            Arc::clone(&self.budget),
            self.notes.0.clone(),
            self.clock.clone(),
        ))
    }

    /// True while `snapshot` is the Ready view's version at this generation.
    fn current(&self, snapshot: &LaunchSnapshot) -> bool {
        matches!(self.view, ClaudeCodeView::Ready { version, .. } if version == snapshot.version)
            && snapshot.account == self.generation
    }

    /// True while launch `id` is neither revoked nor torn down.
    fn live(&self, id: LaunchId) -> bool {
        self.launches
            .iter()
            .any(|held| held.id == id && !held.control.is_revoked() && !held.control.is_ended())
    }

    /// Admits a launch for `snapshot` with its one report due.
    fn attempt(
        &mut self,
        snapshot: LaunchSnapshot,
        expires: Option<Instant>,
    ) -> Result<Attempt, LaunchRefused> {
        if !self.current(&snapshot) {
            return Err(LaunchRefused::NotReady);
        }
        let reviewed = policy::reviewed(snapshot.version).map_err(|_| LaunchRefused::NotReady)?;
        let base = self
            .lane
            .as_ref()
            .map(|lane| lane.base().to_owned())
            .ok_or(LaunchRefused::NotReady)?;
        let launch = self.launch(Question {
            thread: snapshot.thread,
            model: snapshot.model.clone(),
            effort: snapshot.effort.clone(),
            tools: snapshot.offer.schemas(),
            lifetime: MAX_LAUNCH,
        })?;
        if let Some(held) = self.launches.last_mut() {
            held.due = true;
        }
        let report = Report::new(launch.id(), self.notes.0.clone(), (base, snapshot.version));
        Ok(Attempt::new(snapshot, launch, reviewed, report, expires))
    }

    /// Revokes the launch `id` for a replacement, so its report changes nothing.
    fn replaced(&mut self, id: LaunchId) {
        if let Some(held) = self.launches.iter_mut().find(|held| held.id == id) {
            held.replaced = true;
            held.control.withdraw();
        }
    }

    /// Forgets the launches whose teardown is over, unless an attempt's report that still counts
    /// is to come; one from an older generation still counts for an audit failure.
    fn prune(&mut self) {
        self.launches
            .retain(|held| !held.control.is_ended() || (held.due && !held.replaced));
    }

    /// One of the attempts' notes; true when the view may have changed.
    fn note(&mut self, note: Note) -> bool {
        self.prune();
        match note {
            Note::Ended { id, outcome } => self.ended(id, outcome),
            Note::Analytics { id, on } => {
                let current = self.launches.iter().any(|held| {
                    held.id == id && !held.replaced && held.generation == self.generation
                });
                match &mut self.view {
                    ClaudeCodeView::Ready { analytics_on, .. } if current => {
                        *analytics_on = on;
                        true
                    }
                    _ => false,
                }
            }
            Note::Refresh(refresh) => {
                self.refresh(refresh);
                false
            }
        }
    }

    /// An attempt's one report: an audit failure quarantines its version, and a sign-out shows.
    fn ended(&mut self, id: LaunchId, outcome: launch::Outcome) -> bool {
        let generation = self.generation;
        let Some(held) = self
            .launches
            .iter_mut()
            .find(|held| held.id == id && held.due)
        else {
            return false;
        };
        held.due = false;
        if held.replaced {
            return false;
        }
        let (version, current) = (held.version, held.generation == generation);
        match outcome {
            // An audit failure is evidence about the version, whatever generation it ran under.
            launch::Outcome::Audit(reason) => {
                self.audit_failed(version, reason);
                true
            }
            // A lane that refuses a launch is stuck or closing, whatever generation admitted it.
            launch::Outcome::Refused if matches!(self.view, ClaudeCodeView::Ready { .. }) => {
                self.view = ClaudeCodeView::Unavailable;
                self.revoke_all();
                true
            }
            launch::Outcome::SignedOut
                if current && matches!(self.view, ClaudeCodeView::Ready { .. }) =>
            {
                self.view = ClaudeCodeView::SignedOut { version };
                self.revoke_all();
                true
            }
            _ => false,
        }
    }

    /// A fresh attempt for a round whose warm one expired; one it can't deliver is dropped,
    /// which revokes it.
    fn refresh(
        &mut self,
        Refresh {
            old,
            snapshot,
            reply,
        }: Refresh,
    ) {
        self.replaced(old);
        reply.send(self.attempt(snapshot, None)).ok();
    }

    /// Revokes every launch; each one's teardown runs on the lane before its next call.
    fn revoke_all(&mut self) {
        self.warm = None;
        for held in &self.launches {
            held.control.withdraw();
        }
    }

    /// Revokes a thread's launches as `op` deletes it, before the store sees it.
    pub(crate) fn deleting(&mut self, op: &StoreOp) {
        if let StoreOp::Delete { thread } = op {
            self.revoke_thread(*thread);
        }
    }

    /// Revokes `thread`'s launches.
    fn revoke_thread(&mut self, thread: ThreadId) {
        if self
            .warm
            .as_ref()
            .is_some_and(|warm| warm.snapshot().thread == thread)
        {
            self.warm = None;
        }
        for held in self.launches.iter().filter(|held| held.thread == thread) {
            held.control.withdraw();
        }
    }

    pub(crate) async fn next(&mut self) -> Wake {
        // An unused warm launch would hold the lane, which runs one launch at a time, until its
        // deadline, so it ends at its expiry.
        let expiry = self.warm.as_ref().and_then(Attempt::expires);
        let left = expiry.map(|expiry| expiry.saturating_duration_since(self.clock.now()));
        let Self { lane, notes, .. } = self;
        let lane = async {
            match lane {
                Some(lane) => lane.next().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            done = lane => Wake::Lane(done),
            Some(note) = notes.1.recv() => Wake::Note(note),
            () = tokio::time::sleep(left.unwrap_or_default()), if left.is_some() => Wake::Expired,
        }
    }

    /// What woke it; true when the view may have changed.
    pub(crate) fn wake(&mut self, wake: Wake) -> bool {
        match wake {
            Wake::Lane(done) => self.done(done),
            Wake::Note(note) => self.note(note),
            Wake::Expired => {
                let now = self.clock.now();
                if let Some(warm) = self
                    .warm
                    .take_if(|warm| warm.expires().is_some_and(|expires| now >= expires))
                {
                    self.replaced(warm.id());
                }
                false
            }
        }
    }

    /// Ends the lane by `deadline`; true when nothing it started can still be running.
    pub(crate) async fn close(&mut self, deadline: tokio::time::Instant) -> bool {
        match &mut self.lane {
            Some(lane) => lane.close(deadline).await,
            None => true,
        }
    }
}

/// The view a failure leaves.
fn failed(failure: Failure) -> ClaudeCodeView {
    match failure {
        // The lane is stuck or closing: whatever showed before can't be acted on any more.
        Failure::Refused => ClaudeCodeView::Unavailable,
        Failure::Busy => ClaudeCodeView::InUse,
        Failure::Cleanup => ClaudeCodeView::CleanupPending,
        Failure::Find(FindError::NotFound) => ClaudeCodeView::NotFound,
        Failure::Find(FindError::Shim) => ClaudeCodeView::Unsupported(Unsupported::Shim),
        Failure::Unsupported(unsupported) => ClaudeCodeView::Unsupported(unsupported),
        Failure::Policy(policy) => ClaudeCodeView::PolicyFailed(policy),
        Failure::Deadline => ClaudeCodeView::AuditFailed(AuditFailed::Deadline),
        Failure::Version | Failure::Quarantined(_) => {
            ClaudeCodeView::AuditFailed(AuditFailed::Version)
        }
        Failure::Quarantine
        | Failure::Folders
        | Failure::Spawn
        | Failure::TooLong
        | Failure::Pipe
        | Failure::Status(_) => ClaudeCodeView::AuditFailed(AuditFailed::Malformed),
    }
}

impl Brain {
    /// Both connections as they stand now.
    pub(super) fn connections_frame(&self) -> ControlToShell {
        ControlToShell::Connections(ConnectionsView {
            anthropic: self.account.connections.anthropic(),
            claude_code: self.claude.view(),
        })
    }

    /// A frame for the shell, with any Connections frame put together from both owners now.
    pub(super) fn composed(&self, frame: &ControlToShell) -> ControlToShell {
        match frame {
            ControlToShell::Connections(_) => self.connections_frame(),
            other => other.clone(),
        }
    }

    /// Tells the coordinator whether runs on Claude Code can be answered, after every op and wake
    /// whether or not the view changed, so it's never left a state behind.
    pub(super) fn claude_reported(&mut self, link: Option<&mut Link<'_, '_>>) -> Result<(), Ended> {
        let input = Input::Engines(ConnectionKind::ClaudeCode, self.claude.availability());
        self.hold_fence(&input);
        let effects = self
            .coordinator
            .handle(input)
            .expect("availability is never stale");
        self.apply(effects, link)
    }

    pub(super) fn claude_op(
        &mut self,
        op: ConnectionOp,
        mut link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        self.claude.op(op);
        self.claude_reported(link.as_deref_mut())
            .map_err(|_| Full)?;
        self.repair_default(link.as_deref_mut())?;
        let Some(link) = link else {
            return Ok(());
        };
        link.control(&self.connections_frame())?;
        self.claude_models(link)
    }

    pub(super) fn claude_done(
        &mut self,
        wake: Wake,
        mut link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        let changed = self.claude.wake(wake);
        self.claude_reported(link.as_deref_mut())
            .map_err(|_| Full)?;
        self.repair_default(link.as_deref_mut())?;
        let Some(link) = link else {
            return Ok(());
        };
        if changed {
            link.control(&self.connections_frame())?;
        }
        self.claude_models(link)
    }

    /// Claude Code's models, if the shell hasn't heard them.
    pub(super) fn claude_models(&mut self, link: &mut Link<'_, '_>) -> Result<(), Full> {
        match self.claude.models_unsent() {
            Some(models) => link.data(&models),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod quarantine_tests;
#[cfg(test)]
mod question_tests;
#[cfg(test)]
pub(super) mod tests;
