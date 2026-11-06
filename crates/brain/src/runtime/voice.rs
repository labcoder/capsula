//! The voice parts at run time: the installer, the loads it asks for, and what the shell and the
//! coordinator hear about them.

use std::path::{Path, PathBuf};

use kumochi_protocol::message::ControlToShell;
use kumochi_protocol::voice::VoicePart;
use tokio::sync::mpsc::UnboundedReceiver;

use super::{Brain, Full, Input, Link};
use crate::voice::LoadReport;
use crate::voice::installer::{Durable, Installer, Load};
use crate::voice::pump::Pump;
use crate::voice::stt::SttHandle;
use crate::voice::tts::TtsHandle;

#[derive(Debug)]
pub(crate) struct Voice {
    installer: Installer,
    models: PathBuf,
    stt: Option<SttHandle>,
    /// Each part's reports, in `VoicePart::ALL` order.
    reports: [UnboundedReceiver<LoadReport>; 2],
    /// What the coordinator was last told about spoken replies.
    spoken: bool,
}

impl Voice {
    /// A report that came while nothing read them.
    fn try_next(&mut self) -> Option<(VoicePart, LoadReport)> {
        let [stt, tts] = &mut self.reports;
        stt.try_recv()
            .map(|report| (VoicePart::SpeechInput, report))
            .or_else(|_| {
                tts.try_recv()
                    .map(|report| (VoicePart::SpokenReplies, report))
            })
            .ok()
    }
}

/// The next part's report, or never without voice or once both handles are gone.
pub(super) async fn next(voice: &mut Option<Voice>) -> (VoicePart, LoadReport) {
    let Some(voice) = voice else {
        return std::future::pending().await;
    };
    let [stt, tts] = &mut voice.reports;
    tokio::select! {
        Some(report) = stt.recv() => (VoicePart::SpeechInput, report),
        Some(report) = tts.recv() => (VoicePart::SpokenReplies, report),
        else => std::future::pending().await,
    }
}

/// Whether every file of `part` under `models` has its manifest size.
fn whole(models: &Path, part: VoicePart) -> bool {
    let dir = models.join(part.dir());
    part.files().all(|file| {
        std::fs::metadata(dir.join(file.name))
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() == file.bytes)
    })
}

impl Brain {
    /// Starts voice at brain start, with no network: each installed part whose files have their
    /// sizes loads. `stuck` names the removals whose folders couldn't be deleted. A handle that
    /// didn't start is `None`, and its part fails to load for the session. `reports` are the
    /// handles' report receivers, in `VoicePart::ALL` order.
    pub(crate) fn attach_voice(
        &mut self,
        models: PathBuf,
        stuck: &[VoicePart],
        (stt, tts): (Option<SttHandle>, Option<TtsHandle>),
        reports: [UnboundedReceiver<LoadReport>; 2],
    ) {
        let listed = self.config.installed();
        let durable = VoicePart::ALL.map(|part| {
            if stuck.contains(&part) {
                Durable::Removing
            } else if listed.contains(&part) {
                Durable::Installed
            } else {
                Durable::Absent
            }
        });
        let whole = VoicePart::ALL.map(|part| whole(&models, part));
        let (installer, loads) = Installer::start(durable, whole, [stt.is_some(), tts.is_some()]);
        self.pump = tts.map(Pump::new);
        self.voice = Some(Voice {
            installer,
            models,
            stt,
            reports,
            spoken: false,
        });
        for load in loads {
            self.voice_load(load);
        }
    }

    fn voice_load(&self, Load { part, ask }: Load) {
        let Some(voice) = &self.voice else {
            return;
        };
        let dir = voice.models.join(part.dir());
        match part {
            VoicePart::SpeechInput => {
                if let Some(stt) = &voice.stt {
                    stt.load(dir, ask);
                }
            }
            VoicePart::SpokenReplies => {
                if let Some(pump) = &self.pump {
                    pump.tts().load(dir, ask);
                }
            }
        }
    }

    /// A handle's report: the installer moves, any reload starts, and a changed view goes to the
    /// shell.
    pub(super) fn voice_reported(
        &mut self,
        part: VoicePart,
        report: LoadReport,
        link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        let Some(voice) = &mut self.voice else {
            return Ok(());
        };
        let before = voice.installer.view();
        let load = voice.installer.reported(part, report);
        let view = voice.installer.view();
        if let Some(load) = load {
            self.voice_load(load);
        }
        self.spoken_replies();
        match link {
            Some(link) if view != before => link.control(&ControlToShell::Voice(view)),
            _ => Ok(()),
        }
    }

    /// Tells the coordinator whether runs speak: spoken replies are ready and talk-back is on.
    pub(super) fn spoken_replies(&mut self) {
        let Some(voice) = &mut self.voice else {
            return;
        };
        let loaded =
            voice.installer.ready(VoicePart::SpokenReplies) && self.config.view().talk_back;
        if loaded != voice.spoken {
            voice.spoken = loaded;
            self.offline(Input::SpokenReplies { loaded });
        }
    }

    /// A connection's voice view, after the reports that came while no shell was connected.
    pub(super) fn voice_opened(&mut self, link: &mut Link<'_, '_>) -> Result<(), Full> {
        while let Some((part, report)) = self.voice.as_mut().and_then(Voice::try_next) {
            self.voice_reported(part, report, None)?;
        }
        match &self.voice {
            Some(voice) => link.control(&ControlToShell::Voice(voice.installer.view())),
            None => Ok(()),
        }
    }
}
