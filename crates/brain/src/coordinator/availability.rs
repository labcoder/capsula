//! Whether a run may be answered, and on which connection (docs/development/runs.md,
//! "Availability"). The runtime reports the account's state as [`Availability`]. A run is bound
//! to the connection that was ready when its turn arrived, and it ends as soon as that
//! connection stops being the ready one, even while its turn is still being written. The model check (S4) runs once, on the first build.

use kumochi_protocol::message::{
    Action, ConnectionKind, ErrorKey, ModelChoice, ModelInfo, RunId, UserError,
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

/// The sign-in each connection kind had ready when a turn arrived. A run is bound to one of
/// them at its first build, by its model choice's kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Arrived([Option<ConnectionTag>; 3]);

impl Arrived {
    pub(super) fn get(self, kind: ConnectionKind) -> Option<ConnectionTag> {
        self.0[slot(kind)]
    }

    pub(super) fn is_empty(self) -> bool {
        self.0.iter().all(Option::is_none)
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

pub(super) fn model_unavailable() -> UserError {
    UserError::new(
        ErrorKey::ModelUnavailable,
        Vec::new(),
        vec![Action::OpenSettings],
    )
    .expect("one action is within the limit")
}

impl Coordinator {
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

    /// The `ChatGPT` sign-in `run` arrived under, while it's current or ending: the tag its
    /// stored native items replay under. Never a bearer's: `sending` decides that.
    pub(crate) fn replay_tag(&self, run: RunId) -> Option<ConnectionTag> {
        self.current
            .iter()
            .chain(&self.ending)
            .find(|found| found.id == run)
            .and_then(|found| found.arrived.get(ConnectionKind::ChatGpt))
    }

    /// The `ChatGPT` sign-in a send under `key` may take the bearer of: a round's, only while its
    /// run is current and bound to `ChatGPT`, and the ready one for a job.
    pub(crate) fn sending(&self, key: SendKey) -> Option<ConnectionTag> {
        match key {
            SendKey::Build(key) => self
                .current
                .as_ref()
                .filter(|run| run.id == key.run)
                .and_then(|run| run.route)
                .filter(|route| route.kind == ConnectionKind::ChatGpt)
                .map(|route| route.tag),
            SendKey::Job(_) => self.connection(),
        }
    }

    /// `ChatGPT`'s ready sign-in.
    pub(super) fn connection(&self) -> Option<ConnectionTag> {
        match &self.availability {
            Availability::Ready(ready) => Some(ready.connection),
            Availability::Unavailable(_) => None,
        }
    }

    /// Why a run on `route` can't go on, if it can't: another sign-in on its kind needs a sign-in,
    /// and an unavailable kind gives its own reason.
    pub(super) fn refusal(&self, route: Route) -> Option<UserError> {
        match self.availability_of(route.kind) {
            Availability::Ready(ready) if ready.connection == route.tag => None,
            Availability::Ready(_) => Some(sign_in_needed()),
            Availability::Unavailable(reason) => Some(reason.clone()),
        }
    }

    /// The route a run that arrived with `arrived` takes for `choice`: the choice's kind, else
    /// `ChatGPT`'s, under the sign-in that kind had at arrival. A kind that wasn't ready then is
    /// refused even when it's ready now, so a later sign-in never answers the run.
    pub(super) fn route_for(
        &self,
        arrived: Arrived,
        choice: Option<&ModelChoice>,
    ) -> Result<Route, UserError> {
        let kind = choice.map_or(ConnectionKind::ChatGpt, |choice| choice.connection);
        match (arrived.get(kind), self.availability_of(kind)) {
            (Some(tag), _) => Ok(Route { kind, tag }),
            (None, Availability::Unavailable(reason)) => Err(reason.clone()),
            (None, Availability::Ready(_)) => Err(sign_in_needed()),
        }
    }

    /// S4: a run's model must be in its route's catalog, as its first build read it. The
    /// thread's choice is never changed, and nothing goes to another model.
    /// shortcut: the catalog lists no efforts, so the effort isn't checked; 18b adds them.
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
