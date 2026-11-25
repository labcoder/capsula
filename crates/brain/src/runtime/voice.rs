//! The voice parts at run time: the installer, the work it asks for, and what the shell and the
//! coordinator hear about them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kumochi_protocol::message::{
    ControlToShell, ErrorKey, HoldFailure, HoldStatus, SttUnload, VoiceView,
};
use kumochi_protocol::voice::VoicePart;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::settings::{BRAIN, Change, Queued, VoiceWrite};
use super::{Brain, Full, Input, Link};
use crate::platform::{self, Untrusted};
use crate::voice::LoadReport;
use crate::voice::download::{DownloadError, Fetch, FetchPhase, Progress};
use crate::voice::installer::Durable;
use crate::voice::installer::{self, Effect, Folder, Installer, RemoveAnswer};
use crate::voice::pump::Pump;
use crate::voice::stt::SttHandle;
use crate::voice::tts::TtsHandle;

#[cfg(all(test, windows))]
mod path_tests;

pub(crate) struct Voice {
    installer: Installer,
    models: PathBuf,
    stt: Option<SttHandle>,
    /// Each part's reports, in `VoicePart::ALL` order.
    reports: [UnboundedReceiver<LoadReport>; 2],
    fetcher: Arc<dyn Fetch>,
    /// The one fetch running, cancelled or not.
    fetch: Option<Fetching>,
    /// Each part's delete in flight, in `VoicePart::ALL` order.
    deletes: [Option<Deleting>; 2],
    /// What the coordinator was last told about spoken replies.
    spoken: bool,
    pub(super) deferred_talk_back: bool,
}

impl std::fmt::Debug for Voice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Voice")
            .field("view", &self.installer.view())
            .field("fetching", &self.fetch.is_some())
            .field("spoken", &self.spoken)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct Fetching {
    part: VoicePart,
    token: u64,
    progress: watch::Receiver<Progress>,
    cancel: watch::Sender<bool>,
    task: JoinHandle<Result<(), DownloadError>>,
}

#[derive(Debug)]
struct Deleting {
    token: u64,
    task: JoinHandle<Folder>,
}

#[cfg(test)]
pub(super) struct Diagnostic<'a>(&'a Voice);

#[cfg(test)]
impl std::fmt::Debug for Diagnostic<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let voice = self.0;
        f.debug_struct("Voice")
            .field("parts", &voice.installer.diagnostics())
            .field(
                "fetch(part,token,finished)",
                &voice
                    .fetch
                    .as_ref()
                    .map(|fetch| (fetch.part, fetch.token, fetch.task.is_finished())),
            )
            .field(
                "deletes(token,finished)",
                &voice.deletes.each_ref().map(|delete| {
                    delete
                        .as_ref()
                        .map(|delete| (delete.token, delete.task.is_finished()))
                }),
            )
            .field(
                "queued_reports",
                &voice.reports.each_ref().map(UnboundedReceiver::len),
            )
            .finish()
    }
}

/// Something voice heard.
#[derive(Clone, Copy, Debug)]
pub(super) enum Event {
    Report(VoicePart, LoadReport),
    /// The fetch running has new progress.
    Progress,
    /// The fetch running ended.
    Fetched(Result<(), DownloadError>),
    /// A part's delete ended, with the folder as it left it.
    Deleted(VoicePart, Folder),
}

impl Voice {
    #[cfg(test)]
    pub(super) fn diagnostics(&self) -> Diagnostic<'_> {
        Diagnostic(self)
    }

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
    let [first, second] = &mut voice.deletes;
    let (progress, task) = match &mut voice.fetch {
        Some(fetch) => (Some(&mut fetch.progress), Some(&mut fetch.task)),
        None => (None, None),
    };
    tokio::select! {
        Some(report) = stt.recv() => Event::Report(VoicePart::SpeechInput, report),
        Some(report) = tts.recv() => Event::Report(VoicePart::SpokenReplies, report),
        Some(()) = changed(progress) => Event::Progress,
        Some(done) = joined(task) => Event::Fetched(done),
        Some(done) = deleted(first) => Event::Deleted(VoicePart::SpeechInput, done),
        Some(done) = deleted(second) => Event::Deleted(VoicePart::SpokenReplies, done),
        else => std::future::pending().await,
    }
}

async fn changed(progress: Option<&mut watch::Receiver<Progress>>) -> Option<()> {
    progress?.changed().await.ok()
}

// shortcut: `joined` and `deleted` leave a finished task borrowed until `voice_event` takes it; clear it here if anything else polls.
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

async fn deleted(delete: &mut Option<Deleting>) -> Option<Folder> {
    let done = (&mut delete.as_mut()?.task).await;
    Some(done.unwrap_or_else(|error| {
        tracing::error!(panicked = error.is_panic(), "a voice delete didn't finish");
        Folder::Present
    }))
}

/// Deletes `part`'s folder under `models`, and says whether it's still there. Every folder from the
/// volume root down to `models` is held while the part's folder is deleted through handles that
/// never follow a reparse point, so a junction on the way, or a folder someone else could change,
/// deletes nothing. A folder that's already gone counts as deleted. It blocks.
pub(super) fn delete(models: &Path, part: VoicePart) -> Folder {
    let deleted =
        match platform::validate_model_path(models).and_then(|()| platform::hold_chain(models)) {
            Ok(_held) => platform::remove_model_tree(&models.join(part.dir())),
            Err(Untrusted::Missing) => Ok(()),
            Err(error) => Err(error),
        };
    match deleted {
        Ok(()) => Folder::Missing,
        Err(error) => {
            tracing::warn!(?part, ?error, "a removed voice part wasn't deleted");
            Folder::Refused(if platform::is_unsupported(error) {
                ErrorKey::UnsupportedPlatform
            } else {
                ErrorKey::VoiceStorage
            })
        }
    }
}

/// Whether every file of `part` under `models` has its manifest size.
fn whole(models: &Path, part: VoicePart) -> Result<bool, ErrorKey> {
    let dir = models.join(part.dir());
    let manifest = part.files().collect::<Vec<_>>();
    let names = manifest.iter().map(|file| file.name).collect::<Vec<_>>();
    let held = match platform::ModelFiles::open(&dir, &names) {
        Ok(held) => held,
        Err(Untrusted::Missing) => return Ok(false),
        Err(Untrusted::Reparse | Untrusted::Unsafe) => return Err(ErrorKey::VoiceModelPath),
        Err(error) if platform::is_unsupported(error) => {
            return Err(ErrorKey::UnsupportedPlatform);
        }
        Err(_) => return Err(ErrorKey::VoiceStorage),
    };
    for (file, pin) in held.files.iter().zip(manifest) {
        if file.metadata().map_err(|_| ErrorKey::VoiceStorage)?.len() != pin.bytes {
            return Ok(false);
        }
    }
    Ok(true)
}

impl Brain {
    pub(super) fn hold_voice(&self) -> (kumochi_protocol::message::PartState, Option<&SttHandle>) {
        self.voice.as_ref().map_or(
            (kumochi_protocol::message::PartState::NotInstalled, None),
            |voice| (voice.installer.view().speech_input, voice.stt.as_ref()),
        )
    }

    #[cfg(test)]
    pub(super) fn voice_deleting(&self, part: VoicePart) -> bool {
        self.voice
            .as_ref()
            .is_some_and(|voice| voice.deletes[installer::index(part)].is_some())
    }

    /// Starts voice at brain start, with no network: installed parts with complete files load
    /// except speech input when Hold to talk is off. `stuck` names the removals whose folders
    /// couldn't be deleted; every other
    /// unfinished removal has its `RemoveDone` queued. A handle that didn't start is `None`, and
    /// its part fails to load for the session. `reports` are the handles' report receivers, in
    /// `VoicePart::ALL` order, and `fetcher` fetches what an install asks for.
    pub(crate) fn attach_voice(
        &mut self,
        models: PathBuf,
        stuck: &[(VoicePart, Folder)],
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
        let (installer, effects) = Installer::start(
            durable,
            whole,
            [stt.is_some(), tts.is_some()],
            stuck,
            self.config.view().hold_to_talk,
        );
        self.pump = tts.map(Pump::new);
        self.voice = Some(Voice {
            installer,
            models,
            stt,
            reports,
            fetcher,
            fetch: None,
            deletes: [None, None],
            spoken: false,
            deferred_talk_back: false,
        });
        self.run(effects);
        self.voice_policy();
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
                Effect::Delete { part, token } => {
                    let models = voice.models.clone();
                    let task = tokio::task::spawn_blocking(move || delete(&models, part));
                    voice.deletes[installer::index(part)] = Some(Deleting { token, task });
                }
                Effect::Clear { part } => self.settings_ops.push_back(Queued {
                    change: Change::Voice(VoiceWrite::RemoveDone(part)),
                    epoch: BRAIN,
                }),
            }
        }
    }

    /// Carries out `effects`, then tells the coordinator whether runs speak and the shell the
    /// view, if it changed from `before`. A write it queues waits for the next settings pump.
    pub(super) fn voice_apply(
        &mut self,
        before: VoiceView,
        effects: Vec<Effect>,
        mut link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        self.run(effects);
        self.voice_policy();
        if let Some(link) = link.as_deref_mut() {
            if link.registration.is_some() && self.hold_consent_pending() {
                self.hold_end(HoldStatus::Failed(HoldFailure::ChoiceChanged), link)?;
            } else {
                self.hold_voice_changed(link)?;
            }
        }
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
            Event::Deleted(part, done) => match voice.deletes[installer::index(part)].take() {
                Some(delete) => voice.installer.deleted(part, delete.token, done),
                None => Vec::new(),
            },
        };
        self.voice_apply(before, effects, link)
    }

    /// Applies an installer action; its caller owns any settings acknowledgement.
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

    pub(super) fn fence_voice_installs(&mut self) -> Option<VoiceView> {
        let voice = self.voice.as_mut()?;
        let before = voice.installer.view();
        let effects = voice.installer.cancel();
        self.run(effects);
        self.voice_policy();
        Some(before)
    }

    pub(super) fn voice_input_load_refusal(&self) -> Option<ErrorKey> {
        self.voice.as_ref().map_or_else(
            || {
                Some(if self.config.view().hold_to_talk {
                    ErrorKey::VoiceUnavailable
                } else {
                    ErrorKey::HoldToTalkOff
                })
            },
            |voice| voice.installer.load_input_refusal(),
        )
    }

    /// Whether the installer still wants the Activate write for `token`.
    pub(super) fn voice_activating(&self, part: VoicePart, token: u64) -> bool {
        self.voice
            .as_ref()
            .is_some_and(|voice| voice.installer.activating(part, token))
    }

    /// `RemoveVoice`: the installer's answer, the view before it and the effects still to carry
    /// out, or `None` without voice.
    pub(super) fn voice_remove(
        &mut self,
        part: VoicePart,
    ) -> Option<(RemoveAnswer, VoiceView, Vec<Effect>)> {
        let voice = self.voice.as_mut()?;
        let before = voice.installer.view();
        let folder = match platform::validate_model_path(&voice.models)
            .and_then(|()| platform::hold_chain(&voice.models.join(part.dir())))
        {
            Err(Untrusted::Missing) => Folder::Missing,
            Err(error) if platform::is_unsupported(error) => {
                Folder::Refused(ErrorKey::UnsupportedPlatform)
            }
            Ok(_) | Err(_) => Folder::Present,
        };
        let (answer, effects) = voice.installer.remove(part, folder);
        Some((answer, before, effects))
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
                VoiceWrite::RemoveIntent { part, token } => installer.intended(part, token, result),
                VoiceWrite::RemoveDone(part) => installer.cleared(part, result),
            },
            link,
        )
    }

    /// Quit's voice boundary, before settings drain: no install starts from here on, and the
    /// running fetch is told to stop.
    pub(super) fn voice_quit(&mut self) {
        let Some(voice) = &mut self.voice else {
            return;
        };
        let effects = voice.installer.quit();
        self.run(effects);
    }

    /// Applies committed idle settings and tells the coordinator whether runs speak.
    pub(super) fn voice_policy(&mut self) {
        let Some(voice) = &mut self.voice else {
            return;
        };
        if let Some(stt) = &voice.stt {
            stt.set_unload(if voice.installer.ready(VoicePart::SpeechInput) {
                self.config.view().stt_unload
            } else {
                SttUnload::Kept
            });
        }
        let loaded = voice.installer.ready(VoicePart::SpokenReplies)
            && self.config.view().talk_back
            && !voice.deferred_talk_back;
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
