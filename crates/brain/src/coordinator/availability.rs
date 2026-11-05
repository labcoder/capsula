//! Whether a run may be answered, and on which connection (docs/development/runs.md,
//! "Availability"). The runtime reports the account's state as [`Availability`]. A run is bound
//! to the connection that was ready when its turn arrived, and it ends as soon as that
//! connection stops being the ready one, even while its turn is still being written. The model check (S4) runs once, on the first build.

use kumochi_protocol::message::{Action, ErrorKey, ModelChoice, ModelInfo, RunId, UserError};

use super::{Coordinator, Effect, SendKey, Stale};
use crate::connections::model::error::Failure as Stream;
use crate::context::ConnectionTag;
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

/// The reason a run can't go on without a signed-in account.
pub(crate) fn sign_in_needed() -> UserError {
    UserError::new(ErrorKey::SignInNeeded, Vec::new(), vec![Action::SignIn])
        .expect("one action is within the limit")
}

fn model_unavailable() -> UserError {
    UserError::new(
        ErrorKey::ModelUnavailable,
        Vec::new(),
        vec![Action::OpenSettings],
    )
    .expect("one action is within the limit")
}

impl Coordinator {
    /// The account changed. A catalog-only change keeps the current run; any other change ends
    /// a run bound to the old connection, so it sends nothing more.
    pub(super) fn available(&mut self, now: Availability, effects: &mut Vec<Effect>) {
        self.availability = now;
        let Some((run, Some(connection))) = self
            .current
            .as_ref()
            .map(|current| (current.id, current.connection))
        else {
            return;
        };
        if let Some(reason) = self.refusal(connection) {
            self.fail_with(run, Failure::Engine, reason, effects)
                .expect("the run is current");
        }
    }

    /// The connection `run` is bound to, while it's current or ending.
    pub(crate) fn bound(&self, run: RunId) -> Option<ConnectionTag> {
        self.current
            .iter()
            .chain(&self.ending)
            .find(|found| found.id == run)
            .and_then(|found| found.connection)
    }

    /// The connection a send under `key` may take the bearer of: a round's run's own, only while
    /// that run is current, and the ready one for a job.
    pub(crate) fn sending(&self, key: SendKey) -> Option<ConnectionTag> {
        match key {
            SendKey::Build(key) if self.current_run() == Some(key.run) => self.bound(key.run),
            SendKey::Build(_) => None,
            SendKey::Job(_) => self.connection(),
        }
    }

    /// The ready connection, which a new turn's run is bound to.
    pub(super) fn connection(&self) -> Option<ConnectionTag> {
        match &self.availability {
            Availability::Ready(ready) => Some(ready.connection),
            Availability::Unavailable(_) => None,
        }
    }

    /// Why a run bound to `connection` can't go on, if it can't.
    pub(super) fn refusal(&self, connection: ConnectionTag) -> Option<UserError> {
        match &self.availability {
            Availability::Ready(ready) if ready.connection == connection => None,
            Availability::Ready(_) => Some(sign_in_needed()),
            Availability::Unavailable(reason) => Some(reason.clone()),
        }
    }

    /// S4: a run's model must be in the catalog its first build ran against. The thread's
    /// choice is never changed, and nothing goes to another model.
    /// shortcut: the catalog lists no efforts, so the effort isn't checked; 18b adds them.
    pub(super) fn unlisted(&self, choice: Option<&ModelChoice>) -> Option<UserError> {
        let Availability::Ready(ready) = &self.availability else {
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
    pub(super) fn engine_reason(
        &self,
        connection: Option<ConnectionTag>,
        finish: Finish,
    ) -> Option<UserError> {
        match finish {
            Finish::Truncated => Some(Stream::Truncated.user_error()),
            Finish::Failed(EngineFailure::Stream(failure)) => Some(failure.user_error()),
            Finish::Failed(EngineFailure::Anthropic(failure)) => Some(failure.user_error()),
            Finish::Failed(EngineFailure::Access(Some(error))) => Some(error),
            Finish::Failed(EngineFailure::Access(None)) => Some(
                connection
                    .and_then(|connection| self.refusal(connection))
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
