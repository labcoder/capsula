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
use crate::connections::github::device::DefaultRepo;
use crate::voice::installer::{Folder, RemoveAnswer};

/// The epoch of a change the brain makes itself. Connections count from 1, so a shell hears only
/// the settled settings.
pub(super) const BRAIN: u64 = 0;

/// A change to `config.toml`: the user's, or one the voice installer makes.
#[derive(Debug)]
pub(super) enum Change {
    User(SettingsOp),
    Voice(VoiceWrite),
    GitHub(GitHubWrite),
}

#[derive(Clone, Debug)]
pub(super) struct GitHubWrite {
    pub(super) generation: u64,
    pub(super) default: DefaultRepo,
}

/// A write the voice installer makes; its result goes back to the installer.
#[derive(Clone, Copy, Debug)]
pub(super) enum VoiceWrite {
    /// Adds a part to `installed`, for the install with this token.
    Activate { part: VoicePart, token: u64 },
    /// Moves a part from `installed` to `removing`, for the removal with this token.
    RemoveIntent { part: VoicePart, token: u64 },
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
    model: bool,
    disables_hold: bool,
    voice: Option<VoiceWrite>,
    github: Option<GitHubWrite>,
    handle: JoinHandle<(Pending, Result<(), ConfigError>)>,
}

#[cfg(test)]
impl Write {
    pub(super) fn diagnostics(&self) -> (Option<VoiceWrite>, bool) {
        (self.voice, self.handle.is_finished())
    }
}

/// What a held test write does once released.
#[cfg(test)]
#[derive(Debug)]
pub(crate) enum Gate {
    /// Write as usual.
    Write,
    WriteAndSignal(tokio::sync::oneshot::Sender<bool>),
    /// Panic with the test sentinel, as a bug in the write would.
    Panic,
}

/// Whether a change became the settings, and why not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Applied,
    /// Taken with no write, so the settings didn't change.
    Accepted,
    Refused(ErrorKey),
}

/// How a write ended, the connection whose change it was, and the installer's write it was.
pub(super) type Written = (
    u64,
    Option<VoiceWrite>,
    Option<GitHubWrite>,
    Result<(Pending, Result<(), ConfigError>), JoinError>,
);

impl Brain {
    pub(super) fn hold_to_talk_enabled(&self) -> bool {
        self.config.view().hold_to_talk
            && self
                .config_write
                .as_ref()
                .is_none_or(|write| !write.disables_hold)
            && !self.settings_ops.iter().any(|queued| {
                matches!(
                    queued.change,
                    Change::User(SettingsOp::SetHoldToTalk(false))
                )
            })
    }

    pub(super) fn changing_model(&self) -> bool {
        self.config_write.as_ref().is_some_and(|write| write.model)
            || self
                .settings_ops
                .iter()
                .any(|queued| matches!(queued.change, Change::User(SettingsOp::SetDefaultModel(_))))
    }

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

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the model-operation bit remains in flight through this settings write"
    )]
    fn start_settings_write(
        &mut self,
        pending: Pending,
        epoch: u64,
        model: bool,
        disables_hold: bool,
        voice: Option<VoiceWrite>,
        github: Option<GitHubWrite>,
    ) {
        #[cfg(test)]
        let gate = self.config_gate.take();
        let handle = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            let completed = held(gate);
            let written = pending.write();
            #[cfg(test)]
            if let Some(completed) = completed {
                completed
                    .send(written.is_ok())
                    .expect("write completion is observed");
            }
            (pending, written)
        });
        self.config_write = Some(Write {
            epoch,
            model,
            disables_hold,
            voice,
            github,
            handle,
        });
    }

    /// Starts the next waiting settings change, unless a write is in flight. A refused one is
    /// settled at once with the settings unchanged.
    #[expect(clippy::too_many_lines, reason = "one ordered settings write pump")]
    pub(super) fn pump_settings(
        &mut self,
        mut link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        while self.config_write.is_none() {
            let Some(Queued { mut change, epoch }) = self.settings_ops.pop_front() else {
                break;
            };
            // A queued repair is recomputed here, so it never replaces a choice written since; with
            // nothing left to repair it changes nothing, pending holds included.
            if epoch == BRAIN
                && let Change::User(SettingsOp::SetDefaultModel(choice)) = &mut change
            {
                let Some(repaired) = self.repaired_default() else {
                    continue;
                };
                *choice = repaired;
            }
            let model = matches!(&change, Change::User(SettingsOp::SetDefaultModel(_)));
            let disables_hold = matches!(&change, Change::User(SettingsOp::SetHoldToTalk(false)));
            if model {
                self.holds
                    .invalidate(kumochi_protocol::message::HoldStatus::Failed(
                        kumochi_protocol::message::HoldFailure::ChoiceChanged,
                    ));
                if let Some(link) = link.as_deref_mut() {
                    self.hold_voice_changed(link)?;
                }
                let effects = self.coordinator.cancel_current_hold();
                self.apply(effects, link.as_deref_mut()).map_err(|_| Full)?;
            }
            let (staged, voice, github) = match change {
                // Popped, so it applies even when its answer can't be sent; no Voice frame
                // goes ahead of a missing answer.
                Change::User(SettingsOp::InstallVoice(part)) if self.voice.is_some() => {
                    let answered = self.settled(epoch, Outcome::Accepted, link.as_deref_mut());
                    let link = if answered.is_ok() {
                        link.as_deref_mut()
                    } else {
                        None
                    };
                    self.voice_op(|installer| installer.install(part), link)?;
                    answered?;
                    continue;
                }
                Change::User(SettingsOp::CancelVoiceInstall) if self.voice.is_some() => {
                    let answered = self.settled(epoch, Outcome::Accepted, link.as_deref_mut());
                    let link = if answered.is_ok() {
                        link.as_deref_mut()
                    } else {
                        None
                    };
                    self.voice_op(crate::voice::installer::Installer::cancel, link)?;
                    answered?;
                    continue;
                }
                Change::User(SettingsOp::RemoveVoice(part)) if self.voice.is_some() => {
                    let (answer, before, effects) =
                        self.voice_remove(part).expect("voice is attached");
                    let outcome = match answer {
                        RemoveAnswer::Intent { token } => {
                            // The intent's write answers the op, in the op's place in the queue.
                            self.settings_ops.push_front(Queued {
                                change: Change::Voice(VoiceWrite::RemoveIntent { part, token }),
                                epoch,
                            });
                            self.voice_apply(before, effects, link.as_deref_mut())?;
                            continue;
                        }
                        RemoveAnswer::Accepted => Outcome::Accepted,
                        RemoveAnswer::Refused => Outcome::Refused(ErrorKey::VoiceUnavailable),
                    };
                    let answered = self.settled(epoch, outcome, link.as_deref_mut());
                    let link = if answered.is_ok() {
                        link.as_deref_mut()
                    } else {
                        None
                    };
                    self.voice_apply(before, effects, link)?;
                    answered?;
                    continue;
                }
                Change::User(op) => (self.config.stage(op), None, None),
                // A removal committed since the load: writing it would put the part back.
                Change::Voice(VoiceWrite::Activate { part, token })
                    if !self.voice_activating(part, token) =>
                {
                    continue;
                }
                Change::Voice(write @ VoiceWrite::Activate { part, .. }) => {
                    (self.config.stage_activate(part), Some(write), None)
                }
                Change::Voice(write @ VoiceWrite::RemoveIntent { part, .. }) => {
                    (self.config.stage_remove_intent(part), Some(write), None)
                }
                Change::Voice(write @ VoiceWrite::RemoveDone(part)) => {
                    (self.config.stage_remove_done(part), Some(write), None)
                }
                Change::GitHub(write) => (
                    self.config.stage_github_default(&write.default),
                    None,
                    Some(write),
                ),
            };
            match staged {
                Ok(pending) => {
                    self.start_settings_write(pending, epoch, model, disables_hold, voice, github);
                }
                Err(error) => {
                    tracing::warn!(%error, "settings change refused");
                    if let Some(write) = github {
                        self.github_default_written(
                            write,
                            Err(error.key()),
                            epoch,
                            link.as_deref_mut(),
                        )?;
                        continue;
                    }
                    if let Some(write) = voice {
                        self.voice_written(write, Err(error.key()), link.as_deref_mut())?;
                    }
                    self.settled(epoch, Outcome::Refused(error.key()), link.as_deref_mut())?;
                }
            }
        }
        Ok(())
    }

    /// The write in flight finished: its change becomes the settings only if it's on disk.
    pub(super) fn settings_done(
        &mut self,
        (epoch, voice, github, written): Written,
        mut link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        let outcome = match written {
            Ok((pending, Ok(()))) => {
                let screen_holds = self.config.view().screen_holds;
                self.config.commit(pending);
                self.voice_policy();
                if screen_holds && !self.config.view().screen_holds {
                    self.screen_holds_off();
                }
                #[cfg(test)]
                self.observer.send(super::Seen::SettingsCommitted).ok();
                Outcome::Applied
            }
            Ok((_, Err(error))) => {
                #[cfg(test)]
                if let crate::config::ConfigError::Write(source) = &error {
                    eprintln!(
                        "settings write refused: kind={:?} os_code={:?}",
                        source.kind(),
                        source.raw_os_error()
                    );
                }
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
        if let Some(write) = github {
            self.github_default_written(
                write,
                match outcome {
                    Outcome::Applied => Ok(()),
                    Outcome::Refused(key) => Err(key),
                    Outcome::Accepted => Err(ErrorKey::SettingsRefused),
                },
                epoch,
                link.as_deref_mut(),
            )?;
            return self.pump_settings(link);
        }
        if let Some(write) = voice {
            let result = match outcome {
                Outcome::Refused(key) => Err(key),
                Outcome::Applied | Outcome::Accepted => Ok(()),
            };
            self.voice_written(write, result, link.as_deref_mut())?;
        }
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
            if super::voice::delete(models, part) == Folder::Present {
                stuck.push(part);
                continue;
            }
            self.settings_ops.push_back(Queued {
                change: Change::Voice(VoiceWrite::RemoveDone(part)),
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
fn held(
    gate: Option<std::sync::mpsc::Receiver<Gate>>,
) -> Option<tokio::sync::oneshot::Sender<bool>> {
    match gate.and_then(|gate| gate.recv().ok()) {
        Some(Gate::WriteAndSignal(completed)) => Some(completed),
        Some(Gate::Panic) => panic!("{}", crate::test_support::SENTINEL),
        Some(Gate::Write) | None => None,
    }
}

/// The settings write in flight, once it ends; never while there's none.
pub(super) async fn written(write: &mut Option<Write>) -> Written {
    let Some(Write {
        epoch,
        voice,
        github,
        handle,
        ..
    }) = write.as_mut()
    else {
        return std::future::pending().await;
    };
    let (epoch, voice, github) = (*epoch, *voice, github.clone());
    let done = handle.await;
    *write = None;
    (epoch, voice, github, done)
}
