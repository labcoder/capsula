//! Settings changes on the brain's side: one write in flight off the core loop, and exactly
//! one answer per change, in order, to the connection that sent it. A change outlives its
//! connection: it's still written while no shell is connected and during Quit, and a later
//! connection only hears the settled settings, never an answer meant for another shell.

use kumochi_protocol::message::{ControlToShell, SettingsOp};
use tokio::task::{JoinError, JoinHandle};

use super::{Brain, Full, Link};
use crate::config::{ConfigError, Pending};

/// A settings change waiting its turn, with the connection that sent it.
#[derive(Debug)]
pub(super) struct Queued {
    pub(super) op: SettingsOp,
    pub(super) epoch: u64,
}

/// The one settings write in flight.
#[derive(Debug)]
pub(super) struct Write {
    epoch: u64,
    handle: JoinHandle<(Pending, Result<(), ConfigError>)>,
}

/// What a held test write does once released.
#[cfg(test)]
#[derive(Debug)]
pub(crate) enum Gate {
    /// Write as usual.
    Write,
    /// Panic with the test sentinel, as a bug in the write would.
    Panic,
}

/// Whether a change became the settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Applied,
    Refused,
}

/// How a write ended, and the connection whose change it was.
pub(super) type Written = (u64, Result<(Pending, Result<(), ConfigError>), JoinError>);

impl Brain {
    /// Sends the settings as they are now, answering no change.
    pub(super) fn settings_now(&self, link: &mut Link<'_, '_>) -> Result<(), Full> {
        link.control(&ControlToShell::Settings {
            view: self.config.view().clone(),
            answer: false,
        })
    }

    /// Tells the connected shell how a change from connection `epoch` went: its own change gets
    /// its answer; another connection's gets nothing, unless it changed the settings, and then
    /// only the settled settings.
    fn settled(
        &self,
        epoch: u64,
        outcome: Outcome,
        link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        let Some(link) = link else {
            return Ok(());
        };
        if epoch == self.epoch {
            link.control(&ControlToShell::Settings {
                view: self.config.view().clone(),
                answer: true,
            })
        } else if outcome == Outcome::Applied {
            self.settings_now(link)
        } else {
            Ok(())
        }
    }

    /// Starts the next waiting settings change, unless a write is in flight. A refused one is
    /// settled at once with the settings unchanged.
    pub(super) fn pump_settings(
        &mut self,
        mut link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        while self.config_write.is_none() {
            let Some(Queued { op, epoch }) = self.settings_ops.pop_front() else {
                break;
            };
            match self.config.stage(op) {
                Ok(pending) => {
                    #[cfg(test)]
                    let gate = self.config_gate.take();
                    let handle = tokio::task::spawn_blocking(move || {
                        #[cfg(test)]
                        held(gate);
                        let written = pending.write();
                        (pending, written)
                    });
                    self.config_write = Some(Write { epoch, handle });
                }
                Err(error) => {
                    tracing::warn!(%error, "settings change refused");
                    self.settled(epoch, Outcome::Refused, link.as_deref_mut())?;
                }
            }
        }
        Ok(())
    }

    /// The write in flight finished: its change becomes the settings only if it's on disk.
    pub(super) fn settings_done(
        &mut self,
        (epoch, written): Written,
        mut link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        let outcome = match written {
            Ok((pending, Ok(()))) => {
                self.config.commit(pending);
                Outcome::Applied
            }
            Ok((_, Err(error))) => {
                tracing::warn!(%error, "settings change refused");
                Outcome::Refused
            }
            Err(error) => {
                // A panic's message could quote the settings, so only its kind is logged.
                tracing::error!(
                    panicked = error.is_panic(),
                    cancelled = error.is_cancelled(),
                    "the settings write didn't finish"
                );
                Outcome::Refused
            }
        };
        self.settled(epoch, outcome, link.as_deref_mut())?;
        self.pump_settings(link)
    }

    /// Advances settings with no shell to tell.
    pub(super) fn pump_settings_offline(&mut self) {
        let pumped = self.pump_settings(None);
        debug_assert!(pumped.is_ok(), "settling without a shell can't fail");
    }

    /// Writes every waiting settings change, one after another, with no shell to tell.
    pub(super) async fn drain_settings(&mut self) {
        self.pump_settings_offline();
        while self.config_write.is_some() {
            let done = written(&mut self.config_write).await;
            let settled = self.settings_done(done, None);
            debug_assert!(settled.is_ok(), "settling without a shell can't fail");
        }
    }

    /// Drains the settings by `deadline`; what's still unsaved then is logged, never dropped
    /// silently. A write already started may still finish before the process exits, and the
    /// file is replaced whole either way.
    pub(super) async fn drain_settings_by(&mut self, deadline: tokio::time::Instant) {
        if tokio::time::timeout_at(deadline, self.drain_settings())
            .await
            .is_err()
        {
            let unsaved = self.settings_ops.len() + usize::from(self.config_write.is_some());
            tracing::warn!(
                unsaved,
                "settings changes weren't saved before shutting down"
            );
        }
    }
}

/// Waits for a test to release a held write, panicking as a bug in the write would when asked.
#[cfg(test)]
fn held(gate: Option<std::sync::mpsc::Receiver<Gate>>) {
    if let Some(gate) = gate
        && let Ok(Gate::Panic) = gate.recv()
    {
        panic!("{}", crate::test_support::SENTINEL);
    }
}

/// The settings write in flight, once it ends; never while there's none.
pub(super) async fn written(write: &mut Option<Write>) -> Written {
    let Some(Write { epoch, handle }) = write.as_mut() else {
        return std::future::pending().await;
    };
    let epoch = *epoch;
    let done = handle.await;
    *write = None;
    (epoch, done)
}
