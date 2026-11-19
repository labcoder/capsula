//! Whether a run may be answered, and on which route (docs/development/runs.md, "Availability").
//! The runtime reports each connection kind's state as [`Availability`]. A turn records the
//! sign-in each kind had ready when it arrived, and its first build binds the route of its
//! choice's kind under that sign-in. Before then, a change to a kind takes it out of the arrival
//! and the run ends once none is left; after, only a change to the route's own kind ends it. The
//! model check (S4) runs once, on the first build, against the route's catalog.

use kumochi_protocol::message::{
    Action, ConnectionKind, Effort, ErrorKey, ModelChoice, ModelInfo, RunId, UserError,
    effective_effort,
};

use super::{Coordinator, Effect, SendKey, Stale};
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
    Waiting,
}

/// A usable connection and its catalog.
#[derive(Clone, Debug, PartialEq)]
pub struct Ready {
    /// Names the sign-in that issued the account's token set; it outlives refreshes and restarts.
    pub(crate) connection: ConnectionTag,
    /// The account's catalog; empty until it's read.
    pub(crate) models: Vec<ModelInfo>,
    /// The dev fixture answers, so its draw calls draw though no request offers the tool.
    #[cfg(any(test, debug_assertions))]
    pub(crate) trusted_draw: bool,
}

impl Ready {
    #[cfg(all(windows, debug_assertions))]
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
            trusted_draw: false,
        }
    }

    /// The dev fixture is selected: it answers every run, with no account or catalog.
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn fixture() -> Self {
        Self {
            connection: ConnectionTag(0),
            models: Vec::new(),
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

/// The tag of the Anthropic key a Messages post found refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefusedKey(pub(crate) ConnectionTag);

/// The sign-in each connection kind had ready when a turn arrived. A run is bound to one of
/// them at its first build, by its model choice's kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Arrived([Option<ConnectionTag>; 3]);

impl Arrived {
    pub(super) fn get(self, kind: ConnectionKind) -> Option<ConnectionTag> {
        self.0[slot(kind)]
    }

    /// The tag `choice`'s kind had at arrival, which the run's stored rounds carry; `ChatGPT`'s
    /// for no choice, and `ConnectionTag(0)` when that kind had none (`RunSnapshot::connection`).
    pub(crate) fn replay(self, choice: Option<&ModelChoice>) -> ConnectionTag {
        self.get(choice.map_or(ConnectionKind::ChatGpt, |choice| choice.connection))
            .unwrap_or(ConnectionTag(0))
    }

    pub(super) fn is_empty(self) -> bool {
        self.0.iter().all(Option::is_none)
    }

    /// Only `kind` ready, on `tag`.
    #[cfg(test)]
    pub(crate) fn of(kind: ConnectionKind, tag: ConnectionTag) -> Self {
        let mut arrived = Self::default();
        arrived.0[slot(kind)] = Some(tag);
        arrived
    }
}

const fn slot(kind: ConnectionKind) -> usize {
    match kind {
        ConnectionKind::ChatGpt => 0,
        ConnectionKind::AnthropicKey => 1,
        ConnectionKind::ClaudeCode => 2,
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
    #[cfg(test)]
    pub(crate) fn claude_code(&self) -> &Availability {
        &self.claude_code
    }

    /// Whether runs on `kind` can be answered now.
    pub(super) fn availability_of(&self, kind: ConnectionKind) -> &Availability {
        match kind {
            ConnectionKind::ChatGpt => &self.availability,
            ConnectionKind::AnthropicKey => &self.anthropic_key,
            ConnectionKind::ClaudeCode => &self.claude_code,
        }
    }

    /// The sign-in each kind has ready now, which a new turn records.
    pub(super) fn arrived_now(&self) -> Arrived {
        let mut arrived = Arrived::default();
        for kind in [
            ConnectionKind::ChatGpt,
            ConnectionKind::AnthropicKey,
            ConnectionKind::ClaudeCode,
        ] {
            if let Availability::Ready(ready) = self.availability_of(kind) {
                arrived.0[slot(kind)] = Some(ready.connection);
            }
        }
        arrived
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
        let entry = match kind {
            ConnectionKind::ChatGpt => &mut self.availability,
            ConnectionKind::AnthropicKey => &mut self.anthropic_key,
            ConnectionKind::ClaudeCode => &mut self.claude_code,
        };
        *entry = now;
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
                current.arrived.0[slot(kind)] = None;
                current.arrived.is_empty().then_some(reason)
            }
        };
        if let Some(reason) = reason {
            self.fail_with(run, Failure::Engine, reason, effects)
                .expect("the run is current");
        }
    }

    /// A Messages post found the Anthropic key under `tag` refused. A current run bound to that
    /// key was sending on it, so it ends with the key's rejection before the route change that
    /// follows would end it as unavailable; any other run keeps its own outcome.
    pub(super) fn key_refused(&mut self, tag: ConnectionTag, effects: &mut Vec<Effect>) {
        let refused = Route {
            kind: ConnectionKind::AnthropicKey,
            tag,
        };
        let Some(run) = self
            .current
            .as_ref()
            .filter(|current| current.route == Some(refused))
            .map(|current| current.id)
        else {
            return;
        };
        let rejected = crate::connections::anthropic::error::Failure::Credential.user_error();
        self.fail_with(run, Failure::Engine, rejected, effects)
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
        let found = &mut current.arrived.0[slot(ConnectionKind::ClaudeCode)];
        match &self.claude_code {
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
        if current.finding == Some(Finding::Waiting)
            && !matches!(self.claude_code, Availability::Finding)
        {
            current.finding = None;
            let run = current.id;
            effects.push(Effect::ArmDeadline {
                run,
                after: super::WALL_CLOCK,
            });
            self.build(run, 1, effects);
        }
        true
    }

    /// A typed turn on Claude Code that arrived while it was being found and has no sign-in for it
    /// waits up to [`super::FINDING_WAIT`] for the next report, with no build or snapshot, so its
    /// release is a first build that reads the choice again.
    pub(super) fn waits_for_claude_code(
        &mut self,
        run: RunId,
        choice: Option<&ModelChoice>,
        effects: &mut Vec<Effect>,
    ) -> bool {
        let claude = choice.is_some_and(|choice| choice.connection == ConnectionKind::ClaudeCode);
        let Ok(current) = self.current(run) else {
            return false;
        };
        if !claude || current.finding != Some(Finding::Due) {
            return false;
        }
        current.finding = Some(Finding::Waiting);
        current.build = None;
        current.snapshot = None;
        effects.push(Effect::ArmDeadline {
            run,
            after: super::FINDING_WAIT,
        });
        true
    }

    /// The `ChatGPT` sign-in a send under `key` may take the bearer of: a round's, only while its
    /// run is current and bound to `ChatGPT`, and the ready one for a job, unless the job in the
    /// slot answers on another connection.
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
                .is_none_or(|job| job.key != key || job.route.kind == ConnectionKind::ChatGpt)
                .then(|| self.connection())
                .flatten(),
        }
    }

    /// Why a run on `ChatGPT` can't be answered while `ChatGPT` is unavailable: `ChatGPT`'s reason,
    /// except that `ConnectionNeeded` takes the `ChatGPT` sign-in's place while Claude Code is
    /// installed but signed out (owner, OSC-742). A positive match, so a Claude Code that isn't
    /// found, isn't reported yet, is off or is broken leaves `ChatGPT`'s notice and its Sign in.
    fn nothing_ready(&self, chatgpt: &UserError) -> UserError {
        let signed_out = matches!(
            &self.claude_code,
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
        match &self.availability {
            Availability::Ready(ready) => Some(ready.connection),
            Availability::Unavailable(_) | Availability::Finding => None,
        }
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
        matches!(&self.availability, Availability::Ready(ready) if ready.trusted())
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
            Finish::Failed(EngineFailure::Access(None)) => Some(
                route
                    .and_then(|route| self.refusal(route))
                    .unwrap_or_else(sign_in_needed),
            ),
            Finish::Completed
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
