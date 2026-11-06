//! The voice parts at run time: the installer, the work it asks for, and what the shell and the
//! coordinator hear about them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kumochi_protocol::message::{ControlToShell, ErrorKey, VoiceView};
use kumochi_protocol::voice::VoicePart;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::settings::{BRAIN, Change, Queued, VoiceWrite};
use super::{Brain, Full, Input, Link};
use crate::voice::LoadReport;
use crate::voice::download::{DownloadError, Fetch, FetchPhase, Progress};
use crate::voice::installer::{Durable, Effect, Installer};
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
    fetcher: Arc<dyn Fetch>,
    /// The one fetch running, cancelled or not.
    fetch: Option<Fetching>,
    /// What the coordinator was last told about spoken replies.
    spoken: bool,
}

#[derive(Debug)]
struct Fetching {
    part: VoicePart,
    token: u64,
    progress: watch::Receiver<Progress>,
    cancel: watch::Sender<bool>,
    task: JoinHandle<Result<(), DownloadError>>,
}

/// Something voice heard.
#[derive(Clone, Copy, Debug)]
pub(super) enum Event {
    Report(VoicePart, LoadReport),
    /// The fetch running has new progress.
    Progress,
    /// The fetch running ended.
    Fetched(Result<(), DownloadError>),
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

/// The next thing voice hears, or never without voice or once nothing is left to hear.
pub(super) async fn next(voice: &mut Option<Voice>) -> Event {
    let Some(voice) = voice else {
        return std::future::pending().await;
    };
    let [stt, tts] = &mut voice.reports;
    let (progress, task) = match &mut voice.fetch {
        Some(fetch) => (Some(&mut fetch.progress), Some(&mut fetch.task)),
        None => (None, None),
    };
    tokio::select! {
        Some(report) = stt.recv() => Event::Report(VoicePart::SpeechInput, report),
        Some(report) = tts.recv() => Event::Report(VoicePart::SpokenReplies, report),
        Some(()) = changed(progress) => Event::Progress,
        Some(done) = joined(task) => Event::Fetched(done),
        else => std::future::pending().await,
    }
}

async fn changed(progress: Option<&mut watch::Receiver<Progress>>) -> Option<()> {
    progress?.changed().await.ok()
}

async fn joined(
    task: Option<&mut JoinHandle<Result<(), DownloadError>>>,
) -> Option<Result<(), DownloadError>> {
    let done = task?.await;
    Some(done.unwrap_or_else(|error| {
        // A panic's message could quote a path, so only its kind is logged.
        tracing::error!(panicked = error.is_panic(), "a voice fetch didn't finish");
        Err(DownloadError::Io)
    }))
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
    /// sizes loads. `stuck` names the removals whose folders couldn't be deleted; every other
    /// unfinished removal has its `RemoveDone` queued. A handle that didn't start is `None`, and
    /// its part fails to load for the session. `reports` are the handles' report receivers, in
    /// `VoicePart::ALL` order, and `fetcher` fetches what an install asks for.
    pub(crate) fn attach_voice(
        &mut self,
        models: PathBuf,
        stuck: &[VoicePart],
        (stt, tts): (Option<SttHandle>, Option<TtsHandle>),
        reports: [UnboundedReceiver<LoadReport>; 2],
        fetcher: Arc<dyn Fetch>,
    ) {
        let removing = self.config.removing();
        let listed = self.config.installed();
        let durable = VoicePart::ALL.map(|part| {
            if removing.contains(&part) {
                Durable::Removing
            } else if listed.contains(&part) {
                Durable::Installed
            } else {
                Durable::Absent
            }
        });
        let whole = VoicePart::ALL.map(|part| whole(&models, part));
        let (installer, effects) =
            Installer::start(durable, whole, [stt.is_some(), tts.is_some()], stuck);
        self.pump = tts.map(Pump::new);
        self.voice = Some(Voice {
            installer,
            models,
            stt,
            reports,
            fetcher,
            fetch: None,
            spoken: false,
        });
        self.run(effects);
    }

    /// Carries out the installer's effects.
    fn run(&mut self, effects: Vec<Effect>) {
        let Some(voice) = &mut self.voice else {
            return;
        };
        for effect in effects {
            match effect {
                Effect::Fetch { part, token } => {
                    debug_assert!(voice.fetch.is_none(), "one fetch at a time");
                    let (sender, progress) = watch::channel(Progress {
                        phase: FetchPhase::Verifying,
                        done: 0,
                        total: 0,
                    });
                    let (cancel, halt) = watch::channel(false);
                    let fetcher = Arc::clone(&voice.fetcher);
                    let task =
                        tokio::spawn(async move { fetcher.fetch(part, &sender, &halt).await });
                    voice.fetch = Some(Fetching {
                        part,
                        token,
                        progress,
                        cancel,
                        task,
                    });
                }
                Effect::CancelFetch => {
                    if let Some(fetch) = &voice.fetch {
                        fetch.cancel.send_replace(true);
                    }
                }
                Effect::Load { part, ask } => {
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
                Effect::Unload { part, ask } => match part {
                    VoicePart::SpeechInput => {
                        if let Some(stt) = &voice.stt {
                            stt.unload(ask);
                        }
                    }
                    VoicePart::SpokenReplies => {
                        if let Some(pump) = &self.pump {
                            pump.tts().unload(ask);
                        }
                    }
                },
                Effect::Activate { part, token } => self.settings_ops.push_back(Queued {
                    change: Change::Voice(VoiceWrite::Activate { part, token }),
                    epoch: BRAIN,
                }),
            }
        }
    }

    /// Carries out `effects`, then tells the coordinator whether runs speak and the shell the
    /// view, if it changed from `before`. An Activate it queues waits for the next settings pump.
    fn voice_apply(
        &mut self,
        before: VoiceView,
        effects: Vec<Effect>,
        link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        self.run(effects);
        self.spoken_replies();
        let Some(voice) = &self.voice else {
            return Ok(());
        };
        let view = voice.installer.view();
        match link {
            Some(link) if view != before => link.control(&ControlToShell::Voice(view)),
            _ => Ok(()),
        }
    }

    /// What voice heard moves the installer; its effects start.
    pub(super) fn voice_event(
        &mut self,
        event: Event,
        link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        let Some(voice) = &mut self.voice else {
            return Ok(());
        };
        let before = voice.installer.view();
        let effects = match event {
            Event::Report(part, report) => voice.installer.reported(part, report),
            Event::Progress => {
                if let Some(fetch) = &mut voice.fetch {
                    let value = *fetch.progress.borrow_and_update();
                    voice.installer.progress(fetch.part, fetch.token, value);
                }
                Vec::new()
            }
            Event::Fetched(result) => match voice.fetch.take() {
                Some(fetch) => voice.installer.fetched(fetch.part, fetch.token, result),
                None => Vec::new(),
            },
        };
        self.voice_apply(before, effects, link)
    }

    /// An install or cancel op, already answered accepted.
    pub(super) fn voice_op(
        &mut self,
        op: impl FnOnce(&mut Installer) -> Vec<Effect>,
        link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        let Some(voice) = &mut self.voice else {
            return Ok(());
        };
        let before = voice.installer.view();
        let effects = op(&mut voice.installer);
        self.voice_apply(before, effects, link)
    }

    /// An installer write's result.
    pub(super) fn voice_written(
        &mut self,
        write: VoiceWrite,
        result: Result<(), ErrorKey>,
        link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        self.voice_op(
            |installer| match write {
                VoiceWrite::Activate { part, token } => installer.activated(part, token, result),
                VoiceWrite::RemoveDone(part) => installer.cleared(part, result),
            },
            link,
        )
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

    /// A connection's voice view, after the reports that came while no shell was connected, then
    /// any Activate they queued.
    pub(super) fn voice_opened(&mut self, link: &mut Link<'_, '_>) -> Result<(), Full> {
        while let Some((part, report)) = self.voice.as_mut().and_then(Voice::try_next) {
            self.voice_event(Event::Report(part, report), None)?;
        }
        if let Some(voice) = &self.voice {
            link.control(&ControlToShell::Voice(voice.installer.view()))?;
        }
        self.pump_settings(Some(link))
    }
}
