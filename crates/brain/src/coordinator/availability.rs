//! Whether a run may be answered, and on which route (docs/development/runs.md, "Availability").
//! The runtime reports each connection kind's state as [`Availability`]. A turn records the
//! sign-in each kind had ready when it arrived, and its first build binds the route of its
//! choice's kind under that sign-in. Before then, a change to a kind takes it out of the arrival
//! and the run ends once none is left; after, only a change to the route's own kind ends it. The
//! model check (S4) runs once, on the first build, against the route's catalog.

use kumochi_protocol::message::{
    Action, ConnectionKind, DeliveryProof, Effort, ErrorKey, HistoryFailureCause, ModelChoice,
    ModelInfo, ModelSlug, RunId, UserError, effective_effort,
};

use super::{Coordinator, Effect, SendKey, Stale};
use crate::connections::facts::{self, KINDS, PerKind};
use crate::connections::model::error::Failure as Stream;
use crate::context::{ConnectionTag, Route};
use crate::engines::{EngineFailure, Finish};
use crate::store::Failure;

/// Whether runs can be answered now.
#[derive(Clone, Debug, PartialEq)]
pub enum Availability {
    /// An account is signed in and usable.
    Ready(Ready),
    /// Nothing can answer; the reason is what the user can do about it.
    Unavailable(UserError),
    /// Claude Code is still being found: unavailable with `ModelUnavailable`, except that a typed
    /// turn on it waits for the next report (docs/development/runs.md, "Availability").
    Finding,
}

/// Where a typed turn that arrived while Claude Code was being found stands (OSC-753).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Finding {
    /// Before its first build: `Finding` reports keep it, and Ready records the newest sign-in.
    Due,
    /// Its first build found no sign-in for its Claude Code choice; the next report releases it.
    Waiting { until: tokio::time::Instant },
}

/// A usable connection and its catalog.
#[derive(Clone, Debug, PartialEq)]
pub struct Ready {
    /// Names the sign-in that issued the account's token set; it outlives refreshes and restarts.
    pub(crate) connection: ConnectionTag,
    /// The account's catalog; empty until it's read.
    pub(crate) models: Vec<ModelInfo>,
    /// The listed models a picture may go to: the route's own fact, fresh from its current
    /// sign-in, which for `ChatGPT` is its account's catalog listing image input.
    pub(crate) images: Vec<ModelSlug>,
    /// The dev fixture answers, so its draw calls draw though no request offers the tool.
    #[cfg(any(test, debug_assertions))]
    pub(crate) trusted_draw: bool,
}

impl Ready {
    #[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
    pub(crate) fn draw_replay() -> Self {
        use kumochi_protocol::message::{EffortSet, ModelName, ModelSlug};

        Self {
            connection: ConnectionTag(0),
            models: vec![ModelInfo {
                slug: ModelSlug::new(crate::engines::responses::replay::MODEL)
                    .expect("the replay model has a valid slug"),
                display_name: ModelName::new("Drawing replay")
                    .expect("the replay model has a valid name"),
                supported_efforts: EffortSet::new([Effort::Low]),
            }],
            images: Vec::new(),
            #[cfg(any(test, debug_assertions))]
            trusted_draw: false,
        }
    }

    /// The dev fixture is selected: it answers every run, with no account or catalog.
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn fixture() -> Self {
        Self {
            connection: ConnectionTag(0),
            models: Vec::new(),
            images: Vec::new(),
            trusted_draw: true,
        }
    }

    #[cfg(any(test, debug_assertions))]
    fn trusted(&self) -> bool {
        self.trusted_draw
    }

    #[cfg(not(any(test, debug_assertions)))]
    #[expect(clippy::unused_self, reason = "a release build never trusts a draw")]
    fn trusted(&self) -> bool {
        false
    }
}

impl Availability {
    /// Whether `self` and `other` differ in more than which models take pictures. A picture fact
    /// changing alone never fails a hold or a run: only the picture it covers is withdrawn.
    pub(super) fn differs_beyond_pictures(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Ready(ready), Self::Ready(other)) => {
                ready.connection != other.connection || ready.models != other.models
            }
            _ => self != other,
        }
    }
}

/// Which routes may carry a picture. `ChatGPT` always may, by its catalog's fact; each Claude
/// route only once its [`facts`] row is on, so no setting, message, variable or flag reaches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PictureRoutes {
    anthropic_key: bool,
    claude_code: bool,
}

impl PictureRoutes {
    /// What every build ships.
    pub(crate) const SHIPPED: Self = Self {
        anthropic_key: facts::of(ConnectionKind::AnthropicKey).pictures,
        claude_code: facts::of(ConnectionKind::ClaudeCode).pictures,
    };

    #[cfg(test)]
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "one switch per Claude route"
    )]
    pub(crate) const fn new(anthropic_key: bool, claude_code: bool) -> Self {
        Self {
            anthropic_key,
            claude_code,
        }
    }

    /// Whether a request on `kind` may carry a picture at all.
    pub(crate) const fn allows(self, kind: ConnectionKind) -> bool {
        match kind {
            ConnectionKind::ChatGpt => facts::of(ConnectionKind::ChatGpt).pictures,
            ConnectionKind::AnthropicKey => self.anthropic_key,
            ConnectionKind::ClaudeCode => self.claude_code,
        }
    }
}

/// The Anthropic key a Messages post or a Check found refused, with the post's own attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefusedKey(pub(crate) ConnectionTag, pub(crate) Option<SendKey>);

/// The sign-in each connection kind had ready when a turn arrived, and whether the account
/// fenced `ChatGPT`'s history. A run is bound to one of them at its first build, by its model
/// choice's kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Arrived(PerKind<Option<ConnectionTag>>, bool);

impl Arrived {
    /// Whether `ChatGPT` was fenced, so a build that answers on it reads no history at all.
    pub(crate) fn chatgpt_blocked(self) -> bool {
        self.1
    }

    /// Nothing ready, with `ChatGPT` fenced.
    #[cfg(test)]
    pub(crate) fn blocked() -> Self {
        Self(PerKind::default(), true)
    }

    pub(super) fn get(self, kind: ConnectionKind) -> Option<ConnectionTag> {
        self.0[kind]
    }

    /// The tag `choice`'s kind had at arrival, which the run's stored rounds carry; `ChatGPT`'s
    /// for no choice, and `ConnectionTag(0)` when that kind had none (`RunSnapshot::connection`).
    pub(crate) fn replay(self, choice: Option<&ModelChoice>) -> ConnectionTag {
        self.get(choice.map_or(ConnectionKind::ChatGpt, |choice| choice.connection))
            .unwrap_or(ConnectionTag(0))
    }

    pub(super) fn is_empty(self) -> bool {
        KINDS.into_iter().all(|kind| self.0[kind].is_none())
    }

    /// Only `kind` ready, on `tag`.
    #[cfg(test)]
    pub(crate) fn of(kind: ConnectionKind, tag: ConnectionTag) -> Self {
        let mut arrived = Self::default();
        arrived.0[kind] = Some(tag);
        arrived
    }
}

/// The reason a run can't go on without a signed-in account.
pub(crate) fn sign_in_needed() -> UserError {
    UserError::new(ErrorKey::SignInNeeded, Vec::new(), vec![Action::SignIn])
        .expect("one action is within the limit")
}

/// The reason a turn gets when nothing is connected that could answer it: Open Settings only, since
/// a notice's Sign in starts `ChatGPT`'s sign-in for this key.
pub(crate) fn connection_needed() -> UserError {
    UserError::new(
        ErrorKey::ConnectionNeeded,
        Vec::new(),
        vec![Action::OpenSettings],
    )
    .expect("one action is within the limit")
}

pub(crate) fn model_unavailable() -> UserError {
    UserError::new(
        ErrorKey::ModelUnavailable,
        Vec::new(),
        vec![Action::OpenSettings],
    )
    .expect("one action is within the limit")
}

impl Coordinator {
    /// Whether runs on Claude Code can be answered now.
    pub(crate) fn claude_code(&self) -> &Availability {
        &self.availability[ConnectionKind::ClaudeCode]
    }

    /// Whether runs on `kind` can be answered now.
    pub(crate) fn availability_of(&self, kind: ConnectionKind) -> &Availability {
        &self.availability[kind]
    }

    /// Which routes may carry a picture.
    pub(crate) fn pictures(&self) -> PictureRoutes {
        self.pictures
    }

    #[cfg(test)]
    pub(crate) fn set_pictures(&mut self, pictures: PictureRoutes) {
        self.pictures = pictures;
        self.lose_pictures(None);
        self.revoke_pictures();
    }

    /// Whether a run's picture may go out on `route` to `model` now: the route allows pictures,
    /// the run's own sign-in is still ready, and that sign-in's fresh fact says the model takes
    /// them.
    pub(super) fn pictured(&self, route: Route, model: Option<&ModelSlug>) -> bool {
        matches!(self.availability_of(route.kind), Availability::Ready(ready) if ready.connection == route.tag)
            && model.is_some_and(|model| self.takes_pictures(route.kind, model))
    }

    /// Whether `kind` may take a picture for `model` now, whatever its sign-in: by its switch
    /// and its fresh fact. A route that lists no models, the dev fixture's included, takes none.
    pub(crate) fn takes_pictures(&self, kind: ConnectionKind, model: &ModelSlug) -> bool {
        let Availability::Ready(ready) = self.availability_of(kind) else {
            return false;
        };
        self.pictures.allows(kind) && ready.images.contains(model)
    }

    pub(super) fn picture_models(&self) -> Vec<(Route, Vec<ModelSlug>)> {
        KINDS
            .into_iter()
            .filter_map(|kind| match self.availability_of(kind) {
                Availability::Ready(ready)
                    if self.pictures.allows(kind) && !ready.images.is_empty() =>
                {
                    Some((
                        Route {
                            kind,
                            tag: ready.connection,
                        },
                        ready.images.clone(),
                    ))
                }
                _ => None,
            })
            .collect()
    }

    /// Whether a request on `route` for `choice` may carry its run's picture now, as
    /// [`Self::pictured`] says.
    pub(super) fn request_pictured(
        &self,
        route: Option<Route>,
        choice: Option<&ModelChoice>,
    ) -> bool {
        match (route, choice) {
            (Some(route), Some(choice)) => {
                route.kind == choice.connection && self.pictured(route, Some(&choice.model))
            }
            _ => false,
        }
    }

    /// Whether `run`'s picture may go now, on its route to its chosen model.
    pub(super) fn run_pictured(&self, run: &super::Run) -> bool {
        let choice = run
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.choice.as_ref());
        self.request_pictured(run.route, choice)
    }

    /// The sign-in each kind has ready now, which a new turn records.
    pub(super) fn arrived_now(&self) -> Arrived {
        let mut arrived = Arrived::default();
        for kind in KINDS {
            if let Availability::Ready(ready) = self.availability_of(kind) {
                arrived.0[kind] = Some(ready.connection);
            }
        }
        arrived.1 = self.chatgpt_fenced;
        arrived
    }

    /// Whether the account's sign-in keeps `ChatGPT`'s history from a turn that arrives now: one
    /// whose proof is pending or unproved, ready or not. Only the account lifts it.
    pub(crate) fn fence_chatgpt_reads(&mut self) {
        self.chatgpt_fenced = true;
    }

    /// The account's sign-in no longer fences `ChatGPT`'s history.
    pub(crate) fn lift_chatgpt_fence(&mut self) {
        self.chatgpt_fenced = false;
    }

    /// `kind` changed. A catalog-only change keeps the current run. Any other change ends a run
    /// bound to that kind, and takes the kind out of an unbound run's arrival, which ends the run
    /// once no kind is left; a change to another kind touches neither.
    pub(super) fn available(
        &mut self,
        kind: ConnectionKind,
        now: Availability,
        effects: &mut Vec<Effect>,
    ) {
        self.availability[kind] = now;
        self.lose_pictures(None);
        self.revoke_pictures();
        // A `ChatGPT` job follows the account's sign-in and sign-out instead.
        let job = self.job.as_ref().map(|job| job.route);
        if kind != ConnectionKind::ChatGpt
            && job.is_some_and(|route| route.kind == kind && self.refusal(route).is_some())
        {
            self.cancel_job(effects);
        }
        if kind == ConnectionKind::ClaudeCode && self.claude_code_reported(effects) {
            return;
        }
        let Some((run, route, arrived)) = self
            .current
            .as_ref()
            .map(|current| (current.id, current.route, current.arrived))
        else {
            return;
        };
        let reason = match route {
            Some(route) if route.kind == kind => self.refusal(route),
            Some(_) => None,
            None => {
                let Some(tag) = arrived.get(kind) else {
                    return;
                };
                let Some(reason) = self.refusal(Route { kind, tag }) else {
                    return;
                };
                let current = self.current.as_mut().expect("the run is current");
                current.arrived.0[kind] = None;
                current.arrived.is_empty().then_some(reason)
            }
        };
        if let Some(reason) = reason {
            let current = self.current.as_ref().expect("the run is current");
            let failure = if route.is_some_and(|route| route.kind == ConnectionKind::ChatGpt)
                && super::history_failure::needs_chatgpt_sign_in(reason.key())
            {
                super::history_failure::from_evidence(
                    current,
                    HistoryFailureCause::ChatGptSignIn,
                    current
                        .progress
                        .first_grant
                        .is_none()
                        .then_some(DeliveryProof::NotSent),
                )
            } else {
                Failure::Engine
            };
            self.fail_with(run, failure, reason, effects)
                .expect("the run is current");
        }
    }

    /// A Messages post or a Check found the Anthropic key under `tag` refused. A current run bound
    /// to that key was sending on it, so it ends with the key's rejection before the route change that
    /// follows would end it as unavailable; any other run keeps its own outcome.
    pub(super) fn key_refused(
        &mut self,
        RefusedKey(tag, attempt): RefusedKey,
        effects: &mut Vec<Effect>,
    ) {
        let refused = Route {
            kind: ConnectionKind::AnthropicKey,
            tag,
        };
        let Some(current) = self
            .current
            .as_ref()
            .filter(|current| current.route == Some(refused))
        else {
            return;
        };
        let run = current.id;
        let failure = match attempt {
            Some(SendKey::Build(key))
                if current.started()
                    && !current.finishing
                    && current.build.as_ref().is_some_and(|build| {
                        build.key() == key && build.admitted() && build.inline.is_none()
                    }) =>
            {
                super::history_failure::from_evidence(
                    current,
                    HistoryFailureCause::AnthropicKeyRejected,
                    (key.round == 1 && current.progress.first_grant == Some(key))
                        .then_some(DeliveryProof::NotAccepted),
                )
            }
            _ => Failure::Engine,
        };
        self.close_grants(run);
        let rejected = crate::connections::anthropic::error::Failure::Credential.user_error();
        self.fail_with(run, failure, rejected, effects)
            .expect("the run is current");
    }

    /// A Claude Code report to an unbound run held by [`Finding`]: `Finding` forgets any sign-in
    /// found before and keeps it, Ready records the newest sign-in, any other report before the
    /// first build forgets both, so that build ends the run with its own choice's reason, and any
    /// report but `Finding` releases a waiting run into a new first build. False when no run is
    /// held.
    fn claude_code_reported(&mut self, effects: &mut Vec<Effect>) -> bool {
        let Some(current) = self
            .current
            .as_mut()
            .filter(|run| run.route.is_none() && run.finding.is_some())
        else {
            return false;
        };
        let found = &mut current.arrived.0[ConnectionKind::ClaudeCode];
        match &self.availability[ConnectionKind::ClaudeCode] {
            Availability::Finding => *found = None,
            Availability::Ready(ready) => *found = Some(ready.connection),
            Availability::Unavailable(_) => {
                *found = None;
                if current.finding == Some(Finding::Due) {
                    current.finding = None;
                    return true;
                }
            }
        }
        if let Some(Finding::Waiting { until }) = current.finding
            && !matches!(
                self.availability[ConnectionKind::ClaudeCode],
                Availability::Finding
            )
        {
            let now = tokio::time::Instant::now();
            if now >= until {
                return true;
            }
            current.finding = None;
            let run = current.id;
            let at = now + self.tuning.wall_clock;
            current.control.0.end = at.into_std();
            effects.push(Effect::ArmDeadlineAt { run, at });
            self.build(run, 1, effects);
        }
        true
    }

    /// A typed turn on Claude Code that arrived while it was being found and has no sign-in for it
    /// waits up to `[tuning.runs] finding_wait_s` for the next report, with no build or snapshot, so its
    /// release is a first build that reads the choice again.
    pub(super) fn waits_for_claude_code(
        &mut self,
        run: RunId,
        choice: Option<&ModelChoice>,
        effects: &mut Vec<Effect>,
    ) -> bool {
        let claude = choice.is_some_and(|choice| choice.connection == ConnectionKind::ClaudeCode);
        let wait = self.tuning.finding_wait;
        let Ok(current) = self.current(run) else {
            return false;
        };
        if !claude || current.finding != Some(Finding::Due) {
            return false;
        }
        current.finding = Some(Finding::Waiting {
            until: tokio::time::Instant::now() + wait,
        });
        current.build = None;
        current.snapshot = None;
        effects.push(Effect::ArmDeadline { run, after: wait });
        true
    }

    /// The `ChatGPT` sign-in a send under `key` may take the bearer of: a round's, only while its
    /// run is current and bound to `ChatGPT`, or a job's, only while that exact job is sending on
    /// its ready sign-in.
    pub(crate) fn sending(&self, key: SendKey) -> Option<ConnectionTag> {
        match key {
            SendKey::Build(key) => self
                .current
                .as_ref()
                .filter(|run| run.id == key.run)
                .and_then(|run| run.route)
                .filter(|route| route.kind == ConnectionKind::ChatGpt)
                .map(|route| route.tag),
            SendKey::Job(key) => self
                .job
                .as_ref()
                .filter(|job| job.is_sending(key) && job.route.kind == ConnectionKind::ChatGpt)
                .and_then(|job| self.connection().filter(|tag| *tag == job.route.tag)),
        }
    }

    /// Why a run on `ChatGPT` can't be answered while `ChatGPT` is unavailable: `ChatGPT`'s reason,
    /// except that `ConnectionNeeded` takes the `ChatGPT` sign-in's place while Claude Code is
    /// installed but signed out (owner, OSC-742). A positive match, so a Claude Code that isn't
    /// found, isn't reported yet, is off or is broken leaves `ChatGPT`'s notice and its Sign in.
    fn nothing_ready(&self, chatgpt: &UserError) -> UserError {
        let signed_out = matches!(
            &self.availability[ConnectionKind::ClaudeCode],
            Availability::Unavailable(reason) if reason.key() == ErrorKey::ClaudeCodeSignedOut
        );
        if chatgpt.key() == ErrorKey::SignInNeeded && signed_out {
            connection_needed()
        } else {
            chatgpt.clone()
        }
    }

    /// `ChatGPT`'s ready sign-in.
    pub(super) fn connection(&self) -> Option<ConnectionTag> {
        match &self.availability[ConnectionKind::ChatGpt] {
            Availability::Ready(ready) => Some(ready.connection),
            Availability::Unavailable(_) | Availability::Finding => None,
        }
    }

    /// Whether a send on `route` may go to `model`: on any route but the key's, yes; on the
    /// key's, only while its checked catalog offers `model` now.
    pub(super) fn offered(&self, route: Route, model: Option<&str>) -> bool {
        route.kind != ConnectionKind::AnthropicKey
            || matches!(self.availability_of(route.kind), Availability::Ready(ready)
            if model.is_some_and(|model| {
                ready.models.iter().any(|offered| offered.slug.as_str() == model)
            }))
    }

    /// Why a run on `route` can't go on, if it can't: another sign-in on its kind needs a sign-in
    /// on `ChatGPT` and offers Open Settings on another kind, since a notice's Sign in starts
    /// `ChatGPT`'s; an unavailable kind gives its own reason.
    pub(super) fn refusal(&self, route: Route) -> Option<UserError> {
        match self.availability_of(route.kind) {
            Availability::Ready(ready) if ready.connection == route.tag => None,
            Availability::Ready(_) if route.kind == ConnectionKind::ChatGpt => {
                Some(sign_in_needed())
            }
            Availability::Unavailable(reason) => Some(reason.clone()),
            Availability::Ready(_) | Availability::Finding => Some(model_unavailable()),
        }
    }

    /// The route a run that arrived with `arrived` takes for `choice`: the choice's kind, else
    /// `ChatGPT`'s, under the sign-in that kind had at arrival. A kind that wasn't ready then is
    /// refused with its own reason even when it's ready now, so a later sign-in never answers the
    /// run: `ChatGPT` asks for a sign-in, and another kind offers Open Settings only, since a
    /// notice's Sign in starts `ChatGPT`'s.
    pub(super) fn route_for(
        &self,
        arrived: Arrived,
        choice: Option<&ModelChoice>,
    ) -> Result<Route, UserError> {
        let kind = choice.map_or(ConnectionKind::ChatGpt, |choice| choice.connection);
        match (arrived.get(kind), self.availability_of(kind)) {
            (Some(tag), _) => Ok(Route { kind, tag }),
            (None, Availability::Unavailable(reason)) if kind == ConnectionKind::ChatGpt => {
                Err(self.nothing_ready(reason))
            }
            (None, Availability::Unavailable(reason)) => Err(reason.clone()),
            (None, Availability::Ready(_)) if kind == ConnectionKind::ChatGpt => {
                Err(sign_in_needed())
            }
            (None, Availability::Ready(_) | Availability::Finding) => Err(model_unavailable()),
        }
    }

    /// S4: a run's model must be in its route's catalog, as its first build read it. The
    /// thread's choice is never changed, and nothing goes to another model.
    pub(super) fn unlisted(
        &self,
        kind: ConnectionKind,
        choice: Option<&ModelChoice>,
    ) -> Option<UserError> {
        let Availability::Ready(ready) = self.availability_of(kind) else {
            return Some(sign_in_needed());
        };
        let listed = choice
            .is_some_and(|choice| ready.models.iter().any(|model| model.slug == choice.model));
        (!listed && !ready.trusted()).then(model_unavailable)
    }

    pub(super) fn resolve_chatgpt_effort(
        &self,
        route: Route,
        choice: Option<&ModelChoice>,
    ) -> Option<Effort> {
        let choice = choice.filter(|choice| choice.connection == route.kind)?;
        let Availability::Ready(ready) = self.availability_of(route.kind) else {
            return None;
        };
        if route.tag != ready.connection {
            return None;
        }
        let model = ready
            .models
            .iter()
            .find(|model| model.slug == choice.model)?;
        Some(effective_effort(model.supported_efforts?, choice.effort))
    }

    /// The dev fixture's draw calls draw without being offered.
    pub(super) fn trusted_draw(&self) -> bool {
        matches!(
            &self.availability[ConnectionKind::ChatGpt],
            Availability::Ready(ready) if ready.trusted()
        )
    }

    /// What the shell hears when a round fails: the stream's own meaning, or, when it got no
    /// bearer, the account's own reason, else why the run's connection can't send. The fixture's provider failure and an engine's interruption say nothing.
    pub(super) fn engine_reason(&self, route: Option<Route>, finish: Finish) -> Option<UserError> {
        match finish {
            Finish::Truncated => Some(Stream::Truncated.user_error()),
            Finish::Failed(EngineFailure::Stream(failure)) => Some(failure.user_error()),
            Finish::Failed(EngineFailure::Anthropic(failure)) => Some(failure.user_error()),
            Finish::Failed(EngineFailure::Access(Some(error)) | EngineFailure::Claude(error)) => {
                Some(error)
            }
            Finish::Failed(EngineFailure::ClaudeSignedOut { notice, .. }) => Some(notice),
            Finish::Failed(EngineFailure::Access(None)) => Some(
                route
                    .and_then(|route| self.refusal(route))
                    .unwrap_or_else(sign_in_needed),
            ),
            Finish::Completed
            | Finish::ReceiptOrdered
            | Finish::Interrupted
            | Finish::Failed(
                EngineFailure::Provider | EngineFailure::TurnCap | EngineFailure::ClaudeStream(_),
            ) => None,
        }
    }

    /// Ends the current run as failed, telling the shell why.
    pub(super) fn fail_with(
        &mut self,
        run: RunId,
        failure: Failure,
        reason: UserError,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        self.current(run)?.error = Some(reason);
        self.fail(run, failure, effects)
    }
}
