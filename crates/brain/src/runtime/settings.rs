//! Settings changes on the brain's side: one write in flight off the core loop, and exactly
//! one view back per change, in order.

use kumochi_protocol::message::{ControlToShell, ErrorKey};
use tokio::task::{JoinError, JoinHandle};

use super::{Brain, Full, Link};
use crate::config::{ConfigError, Pending};

/// A staged change being written, and how the write went.
pub(super) type Write = JoinHandle<(Pending, Result<(), ConfigError>)>;

impl Brain {
    /// Sends the settings as they are now.
    pub(super) fn settings_now(&self, link: &mut Link<'_, '_>) -> Result<(), Full> {
        link.control(&ControlToShell::Settings(self.config.view().clone()))
    }

    /// Starts the next waiting settings change, unless a write is in flight. A refused one is
    /// answered at once with the unchanged settings, so every change gets exactly one answer in
    /// order.
    pub(super) fn pump_settings(&mut self, link: &mut Link<'_, '_>) -> Result<(), Full> {
        while self.config_write.is_none() {
            let Some(op) = self.settings_ops.pop_front() else {
                break;
            };
            match self.config.stage(op) {
                Ok(pending) => {
                    #[cfg(test)]
                    let gate = self.config_gate.take();
                    self.config_write = Some(tokio::task::spawn_blocking(move || {
                        #[cfg(test)]
                        if let Some(gate) = gate {
                            gate.recv().ok();
                        }
                        let written = pending.write();
                        (pending, written)
                    }));
                }
                Err(error) => {
                    tracing::warn!(%error, "settings change refused");
                    self.refused(error.key(), link)?;
                }
            }
        }
        Ok(())
    }

    /// The write in flight finished: its change becomes the settings only if it's on disk.
    pub(super) fn settings_done(
        &mut self,
        written: Result<(Pending, Result<(), ConfigError>), JoinError>,
        link: &mut Link<'_, '_>,
    ) -> Result<(), Full> {
        match written {
            Ok((pending, Ok(()))) => {
                self.config.commit(pending);
                self.settings_now(link)?;
            }
            Ok((_, Err(error))) => {
                tracing::warn!(%error, "settings change refused");
                self.refused(error.key(), link)?;
            }
            Err(error) => {
                tracing::error!(%error, "the settings write panicked");
                self.refused(ErrorKey::SettingsRefused, link)?;
            }
        }
        self.pump_settings(link)
    }

    /// Answers a refused change: why, then the unchanged settings.
    fn refused(&self, key: ErrorKey, link: &mut Link<'_, '_>) -> Result<(), Full> {
        link.control(&ControlToShell::Error(key.into()))?;
        self.settings_now(link)
    }
}

/// The settings write in flight, once it ends; never while there's none.
pub(super) async fn written(
    write: &mut Option<Write>,
) -> Result<(Pending, Result<(), ConfigError>), JoinError> {
    let Some(handle) = write.as_mut() else {
        return std::future::pending().await;
    };
    let written = handle.await;
    *write = None;
    written
}
