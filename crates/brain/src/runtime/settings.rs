//! Settings changes on the brain's side: one write in flight off the core loop, and exactly
//! one answer per change, in order, to the connection that sent it. A change outlives its
//! connection: it's still written while no shell is connected and during Quit, and a later
//! connection only hears the settled settings, never an answer meant for another shell.

use std::path::Path;

use kumochi_protocol::message::{ControlToShell, ErrorKey, SettingsOp};
use kumochi_protocol::voice::VoicePart;
use tokio::task::{JoinError, JoinHandle};

use super::{Brain, Full, Link};
use crate::config::{ConfigError, Pending};
use crate::platform::{self, Untrusted};

/// The epoch of a change the brain makes itself. Connections count from 1, so a shell hears only
/// the settled settings.
pub(super) const BRAIN: u64 = 0;

/// A change to `config.toml`: the user's, or one the brain makes for voice.
#[derive(Debug)]
pub(super) enum Change {
    User(SettingsOp),
    /// A part's folder is gone, so its removal is finished.
    RemoveDone(VoicePart),
}

/// A settings change waiting its turn, with the connection that sent it.
#[derive(Debug)]
pub(super) struct Queued {
    pub(super) change: Change,
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

/// Whether a change became the settings, and why not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Applied,
    Refused(ErrorKey),
}

/// How a write ended, and the connection whose change it was.
pub(super) type Written = (u64, Result<(Pending, Result<(), ConfigError>), JoinError>);

impl Brain {
    /// Holds the next settings write before it touches the file, until the returned sender
    /// sends or drops.
    #[cfg(test)]
    pub(crate) fn hold_config_write(&mut self) -> std::sync::mpsc::Sender<Gate> {
        let (release, held) = std::sync::mpsc::channel();
        self.config_gate = Some(held);
        release
    }

    /// Sends the settings as they are now, answering no change.
    pub(super) fn settings_now(&self, link: &mut Link<'_, '_>) -> Result<(), Full> {
        link.control(&ControlToShell::Settings {
            view: self.config.view().clone(),
            answer: false,
        })
    }

    /// Tells the connected shell how a change from connection `epoch` went: its own change gets
    /// its answer, after the refusal's `Error` if it was refused; another connection's gets
    /// nothing, unless it changed the settings, and then only the settled settings.
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
            if let Outcome::Refused(key) = outcome {
                link.control(&ControlToShell::Error(key.into()))?;
            }
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
            let Some(Queued { change, epoch }) = self.settings_ops.pop_front() else {
                break;
            };
            let staged = match change {
                Change::User(op) => self.config.stage(op),
                Change::RemoveDone(part) => self.config.stage_remove_done(part),
            };
            match staged {
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
                    self.settled(epoch, Outcome::Refused(error.key()), link.as_deref_mut())?;
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
                self.spoken_replies();
                #[cfg(test)]
                self.observer.send(super::Seen::SettingsCommitted).ok();
                Outcome::Applied
            }
            Ok((_, Err(error))) => {
                tracing::warn!(%error, "settings change refused");
                Outcome::Refused(error.key())
            }
            Err(error) => {
                // A panic's message could quote the settings, so only its kind is logged.
                tracing::error!(
                    panicked = error.is_panic(),
                    cancelled = error.is_cancelled(),
                    "the settings write didn't finish"
                );
                Outcome::Refused(ErrorKey::SettingsRefused)
            }
        };
        self.settled(epoch, outcome, link.as_deref_mut())?;
        self.pump_settings(link)
    }

    /// Finishes each removal that a crash or a failed delete left: the part's folder under
    /// `models` is deleted, and then its `RemoveDone` is queued. Every folder from the volume root
    /// down to `models` is held while the part's folder is deleted through handles that never
    /// follow a reparse point, so a junction on the way, or a folder someone else could change,
    /// deletes nothing. A folder that's already gone counts as deleted. A part whose folder can't
    /// be deleted stays listed, so the next start tries again. Blocking is fine: it runs at
    /// start-up, before anything is served. Returns the parts left listed.
    pub(crate) fn finish_removals(&mut self, models: &Path) -> Vec<VoicePart> {
        let mut stuck = Vec::new();
        for part in self.config.removing() {
            let deleted = match platform::hold_chain(models) {
                Ok(_held) => platform::remove_tree(&models.join(part.dir())),
                Err(Untrusted::Missing) => Ok(()),
                Err(error) => Err(error),
            };
            if let Err(error) = deleted {
                tracing::warn!(?part, ?error, "a removed voice part wasn't deleted");
                stuck.push(part);
                continue;
            }
            self.settings_ops.push_back(Queued {
                change: Change::RemoveDone(part),
                epoch: BRAIN,
            });
        }
        self.pump_settings_offline();
        stuck
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
