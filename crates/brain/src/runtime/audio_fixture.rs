use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kumochi_protocol::constants::{CONFIG_FILE, SESSION_LOCK};
use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    ControlToBrain, ControlToShell, Cursor, DataToShell, RunId, SceneId, SegmentStart,
    SpeechDecision, SpeechDispatch, SpeechRetirement, StepRef,
};
use tokio::sync::Notify;

use crate::connections::account::Worker;
use crate::connections::clock::{Clock, SystemClock};
use crate::connections::http_client::HttpError;
use crate::connections::oauth::{LaunchError, LaunchTarget, Launcher};
use crate::connections::vault::Vault;
use crate::engines::fixture::Fixture;
use crate::engines::responses::replay::Turns;
use crate::platform::credential::{CredentialStore, CredentialStoreError, SecretBytes};
use crate::voice::download::{DownloadError, Fetch, Progress};
use crate::voice::installer::{Durable, Installer};
use crate::voice::pump::{OutputMode, Pump};
use crate::voice::stt::SttHandle;
use crate::voice::tts::{Engine, TtsError, TtsHandle};
use crate::voice::{LoadEvent, Reports};

use super::account::{Accounts, Net};
use super::data::DataQueue;
use super::{Brain, Config, Input, Selected, Store};

thread_local! {
    static PROBE: std::cell::OnceCell<Arc<AudioProbe>> = const { std::cell::OnceCell::new() };
}

pub(crate) fn install(probe: Arc<AudioProbe>) {
    PROBE.with(|slot| assert!(slot.set(probe).is_ok(), "one fixture per Brain thread"));
}

pub(super) fn probe() -> Option<Arc<AudioProbe>> {
    PROBE.with(|slot| slot.get().cloned())
}

pub(crate) fn accepted_retirement(receipt: SpeechRetirement) {
    if let Some(probe) = probe() {
        probe
            .speech
            .lock()
            .expect("the audio probe is healthy")
            .retired
            .push(receipt);
    }
}

pub(crate) fn accepted_quiet(run: RunId, cursor: Option<Cursor>) {
    if let Some(probe) = probe() {
        probe
            .speech
            .lock()
            .expect("the audio probe is healthy")
            .accepted_quiet = Some((run, cursor));
    }
}

#[derive(Debug)]
struct Offline;

impl Fetch for Offline {
    fn fetch<'a>(
        &'a self,
        _: kumochi_protocol::voice::VoicePart,
        _: &'a tokio::sync::watch::Sender<Progress>,
        _: &'a tokio::sync::watch::Receiver<bool>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), DownloadError>> + Send + 'a>>
    {
        panic!("the offline fixture never fetches voice models")
    }
}

impl Launcher for Offline {
    fn launch(&self, _: &LaunchTarget) -> Result<(), LaunchError> {
        Err(LaunchError::Launch)
    }
}

impl CredentialStore for Offline {
    fn get(&self, _: &str) -> Result<Option<SecretBytes>, CredentialStoreError> {
        Ok(None)
    }

    fn set(&self, _: &str, _: &SecretBytes) -> Result<(), CredentialStoreError> {
        Err(CredentialStoreError::Write(5))
    }

    fn delete(&self, _: &str) -> Result<(), CredentialStoreError> {
        Err(CredentialStoreError::Delete(5))
    }
}

struct Tone(usize);

impl Engine for Tone {
    type Stream = std::vec::IntoIter<Result<Vec<f32>, TtsError>>;

    fn stream(&self, _: &str) -> Result<Self::Stream, TtsError> {
        Ok((0..self.0)
            .map(|_| Ok(vec![0.25; 240]))
            .collect::<Vec<_>>()
            .into_iter())
    }
}

/// Observations and barriers for one synthetic, same-process Brain peer.
#[derive(Debug, Default)]
pub struct AudioProbe {
    text_only: bool,
    hold: bool,
    decoded: AtomicU32,
    received_mic: AtomicU32,
    narrated: Option<OutputMode>,
    initial_credit: Option<u32>,
    replacement_chain: Option<Arc<Notify>>,
    changing_turns: Option<Turns>,
    pub(super) replacement_queued: AtomicBool,
    hold_after_prefix: AtomicBool,
    scene_revoke: Mutex<SceneRevoke>,
    ack: AtomicBool,
    name: AtomicBool,
    control: Notify,
    pub(super) data_blocked: AtomicBool,
    apply_credit: AtomicBool,
    received_credit: AtomicU64,
    applied_credit: AtomicU64,
    pub(super) sent: AtomicU64,
    stop: AtomicU64,
    clears: AtomicU32,
    heard: AtomicU32,
    speech: Mutex<SpeechEvidence>,
    scene_failed: std::sync::Mutex<Option<(RunId, SceneId)>>,
    decision: std::sync::Mutex<Option<(SpeechDispatch, SpeechDecision)>>,
    shown: Mutex<Vec<(RunId, SceneId, u32)>>,
}

#[derive(Debug, Default)]
struct SceneRevoke {
    prefix: Option<SceneId>,
    promise: Option<(StepRef, SpeechDispatch)>,
    announcement: Option<(StepRef, SpeechDispatch)>,
    revoked: bool,
    ends: u32,
    written_announcements: u32,
    successor: Option<(RunId, SceneId)>,
}

#[derive(Debug, Default)]
struct SpeechEvidence {
    retired: Vec<SpeechRetirement>,
    received_retirements: Vec<SpeechRetirement>,
    received_quiet: Option<(RunId, Option<Cursor>)>,
    accepted_quiet: Option<(RunId, Option<Cursor>)>,
}

impl AudioProbe {
    pub(crate) fn text_only() -> Self {
        Self {
            text_only: true,
            ..Self::default()
        }
    }

    pub(crate) fn silent_hold() -> Self {
        Self {
            hold: true,
            ..Self::text_only()
        }
    }

    pub(crate) fn speaking_hold() -> Self {
        Self {
            hold: true,
            ..Self::default()
        }
    }

    /// Number of synthetic utterances the offline recognizer actually decoded.
    #[must_use]
    pub fn decoded(&self) -> u32 {
        self.decoded.load(Ordering::Acquire)
    }

    pub(super) fn mic_received(&self) {
        self.received_mic.fetch_add(1, Ordering::Release);
    }

    /// Number of mic packets received on the data pipe before the ingress gate.
    #[must_use]
    pub fn received_mic(&self) -> u32 {
        self.received_mic.load(Ordering::Acquire)
    }

    pub(crate) fn narrated() -> Self {
        Self {
            narrated: Some(OutputMode::Conducted),
            ..Self::default()
        }
    }

    pub(crate) fn plain_narrated() -> Self {
        Self {
            narrated: Some(OutputMode::Legacy),
            ..Self::narrated()
        }
    }

    pub(crate) fn replacing(credit: u32) -> Self {
        Self {
            initial_credit: Some(credit),
            ..Self::narrated()
        }
    }

    pub(crate) fn chaining_replacements() -> Self {
        Self {
            replacement_chain: Some(Arc::new(Notify::new())),
            ..Self::narrated()
        }
    }

    pub(crate) fn stopped_replacement() -> Self {
        Self {
            changing_turns: Some(Turns::StoppedReplacement),
            ..Self::narrated()
        }
    }

    pub(crate) fn authored_then_narrated() -> Self {
        Self {
            changing_turns: Some(Turns::AuthoredThenNarrated),
            ..Self::narrated()
        }
    }

    pub(super) fn initial_credit(&self) -> Option<u32> {
        self.initial_credit
    }

    /// Whether the Brain has put the real replacement boundary in its data queue.
    #[must_use]
    pub fn replacement_queued(&self) -> bool {
        self.replacement_queued.load(Ordering::Acquire)
    }

    /// Lets the Brain write the registration receipt it already queued.
    pub fn release_ack(&self) {
        self.ack.store(true, Ordering::Release);
        self.control.notify_one();
    }

    /// Lets the Brain write the first authoritative run state.
    pub fn release_run(&self) {
        self.name.store(true, Ordering::Release);
        self.control.notify_one();
    }

    /// The real data writer saw a frame larger than its remaining credit.
    pub fn data_blocked(&self) -> bool {
        self.data_blocked.load(Ordering::Acquire)
    }

    /// Holds the real writer's credit after the next unpromised first step enters its write.
    pub fn hold_after_scene_prefix(&self) {
        self.hold_after_prefix.store(true, Ordering::Release);
    }

    pub(super) fn writing_scene_prefix(&self, queue: &std::cell::RefCell<DataQueue>, frame: &[u8]) {
        if self.hold_after_prefix.load(Ordering::Acquire)
            && let Ok(DataToShell::SceneStepCommit {
                scene,
                index: 0,
                speech: None,
                ..
            }) = frame::decode(&frame[frame::HEADER_LEN..])
        {
            queue.borrow_mut().hold_fixture_credit();
            self.scene_revoke.lock().expect("scene probe").prefix = Some(scene);
            self.hold_after_prefix.store(false, Ordering::Release);
        }
        if matches!(self.changing_turns, Some(Turns::StoppedReplacement))
            && let Ok(DataToShell::SegmentQueued {
                run,
                step: Some(step),
                segment,
                start: SegmentStart::Speech,
                output_generation: output,
            }) = frame::decode(&frame[frame::HEADER_LEN..])
        {
            let mut facts = self.scene_revoke.lock().expect("scene probe");
            if facts.announcement
                == Some((
                    step,
                    SpeechDispatch {
                        run,
                        output,
                        segment,
                    },
                ))
            {
                facts.written_announcements += 1;
            }
        }
    }

    pub(super) fn queued_scene_data(&self, message: &DataToShell) {
        let mut facts = self.scene_revoke.lock().expect("scene probe");
        match message {
            DataToShell::SceneStepCommit {
                run,
                scene,
                index,
                speech: Some(promise),
                ..
            } if self.hold_after_prefix.load(Ordering::Acquire) || facts.prefix == Some(*scene) => {
                facts.promise = Some((
                    StepRef {
                        scene: *scene,
                        index: *index,
                    },
                    SpeechDispatch {
                        run: *run,
                        output: promise.output_generation,
                        segment: promise.segment,
                    },
                ));
            }
            DataToShell::SegmentQueued {
                run,
                step: Some(step),
                segment,
                start: SegmentStart::Speech,
                output_generation,
            } if self.hold_after_prefix.load(Ordering::Acquire)
                || facts.prefix == Some(step.scene) =>
            {
                facts.announcement = Some((
                    *step,
                    SpeechDispatch {
                        run: *run,
                        output: *output_generation,
                        segment: *segment,
                    },
                ));
            }
            DataToShell::RunEnd { run }
                if facts
                    .promise
                    .is_some_and(|(_, promise)| promise.run == *run) =>
            {
                facts.ends += 1;
            }
            DataToShell::SceneStepCommit {
                run,
                scene,
                index: 0,
                speech: None,
                ..
            } if facts.prefix.is_some_and(|prefix| prefix != *scene) => {
                facts.successor = Some((*run, *scene));
            }
            _ => {}
        }
    }

    pub(super) fn applied_scene_revoke(&self, scenes: &[SceneId]) {
        let mut facts = self.scene_revoke.lock().expect("scene probe");
        facts.revoked |= facts.prefix.is_some_and(|prefix| scenes.contains(&prefix));
    }

    /// The exact promise and matching announcement queued behind the held prefix.
    ///
    /// # Panics
    /// Panics if the scene probe is poisoned.
    pub fn held_narration(&self) -> Option<(StepRef, SpeechDispatch)> {
        let facts = self.scene_revoke.lock().expect("scene probe");
        facts
            .promise
            .filter(|promise| Some(*promise) == facts.announcement)
    }

    /// The stopped run's queued end count and written matching announcement count.
    /// # Panics
    /// Panics if the scene probe is poisoned.
    pub fn held_tail(&self) -> (u32, u32) {
        let facts = self.scene_revoke.lock().expect("scene probe");
        (facts.ends, facts.written_announcements)
    }

    /// The next silent scene committed while the prefix's writer credit is held.
    /// # Panics
    /// Panics if the scene probe is poisoned.
    pub fn held_successor(&self) -> Option<(RunId, SceneId)> {
        self.scene_revoke.lock().expect("scene probe").successor
    }

    /// Whether the real queue applied a revoke for the held scene.
    ///
    /// # Panics
    /// Panics if the scene probe is poisoned.
    pub fn applied_held_scene_revoke(&self) -> bool {
        self.scene_revoke.lock().expect("scene probe").revoked
    }

    /// Applies subsequent wire grants to the real Brain data queue.
    pub fn allow_data_credit(&self) {
        self.apply_credit.store(true, Ordering::Release);
    }

    /// Encoded data bytes handed to the pipe, including audio metadata.
    #[must_use]
    pub fn sent_bytes(&self) -> u64 {
        self.sent.load(Ordering::Acquire)
    }

    /// The bytes acknowledged by the Shell on the wire and the bytes the queue actually gained.
    pub fn data_credits(&self) -> (u64, u64) {
        (
            self.received_credit.load(Ordering::Acquire),
            self.applied_credit.load(Ordering::Acquire),
        )
    }

    pub(super) fn credit(&self, queue: &std::cell::RefCell<DataQueue>, ready: &Notify, bytes: u32) {
        self.received_credit
            .fetch_add(u64::from(bytes), Ordering::Release);
        if let Some(hold) = &self.replacement_chain
            && !self.apply_credit.swap(true, Ordering::AcqRel)
        {
            queue.borrow_mut().hold_fixture_credit();
            hold.notify_one();
            return;
        }
        if self.apply_credit.load(Ordering::Acquire) {
            let mut queue = queue.borrow_mut();
            let before = queue.granted_credit();
            queue.grant(bytes);
            let applied = u64::try_from(queue.granted_credit() - before).expect("wire credit fits");
            self.applied_credit.fetch_add(applied, Ordering::Release);
            ready.notify_one();
        }
    }

    /// The run named by the last Stop received on control, or zero.
    pub fn stop(&self) -> RunId {
        RunId(self.stop.load(Ordering::Acquire))
    }

    /// Single-scene clears received on the real Brain control pipe.
    #[must_use]
    pub fn clears(&self) -> u32 {
        self.clears.load(Ordering::Acquire)
    }

    /// The run named by the last quiet receipt received on control, or zero.
    /// # Panics
    /// Panics if the fixture audio recorder is poisoned.
    pub fn quiet(&self) -> RunId {
        self.quiet_receipt().map_or(RunId(0), |(run, _)| run)
    }

    /// Largest heard prefix received on control; this is synthetic audio only.
    pub fn heard(&self) -> u32 {
        self.heard.load(Ordering::Acquire)
    }

    /// The exact last quiet report taken on control, including after natural completion.
    /// # Panics
    /// Panics if the fixture audio recorder is poisoned.
    pub fn quiet_receipt(&self) -> Option<(RunId, Option<Cursor>)> {
        self.speech
            .lock()
            .expect("the audio probe is healthy")
            .received_quiet
    }

    /// The exact retirement frames that changed this run's Brain speech ledger.
    /// # Panics
    /// Panics if the fixture audio recorder is poisoned.
    pub fn retired(&self, run: RunId) -> Vec<SpeechRetirement> {
        self.speech
            .lock()
            .expect("the audio probe is healthy")
            .retired
            .iter()
            .copied()
            .filter(|receipt| receipt.run == run)
            .collect()
    }

    /// The retirement frames received on control for this run, including stale or refused ones.
    /// # Panics
    /// Panics if the fixture audio recorder is poisoned.
    pub fn received_retirements(&self, run: RunId) -> Vec<SpeechRetirement> {
        self.speech
            .lock()
            .expect("the audio probe is healthy")
            .received_retirements
            .iter()
            .copied()
            .filter(|receipt| receipt.run == run)
            .collect()
    }

    /// The last quiet receipt accepted by the Brain, with its exact cursor.
    /// # Panics
    /// Panics if the fixture audio recorder is poisoned.
    pub fn accepted_quiet(&self) -> Option<(RunId, Option<Cursor>)> {
        self.speech
            .lock()
            .expect("the audio probe is healthy")
            .accepted_quiet
    }

    /// The last refused host scene and the Stop already received for it.
    ///
    /// # Panics
    /// Panics if the fixture's control recorder is poisoned.
    pub fn scene_failed(&self) -> Option<(RunId, SceneId)> {
        *self
            .scene_failed
            .lock()
            .expect("the fixture scene recorder is healthy")
    }

    /// The latest exact speech decision received on the real control pipe.
    ///
    /// # Panics
    /// Panics if the fixture's decision recorder is poisoned.
    pub fn speech_decision(&self) -> Option<(SpeechDispatch, SpeechDecision)> {
        *self
            .decision
            .lock()
            .expect("the fixture decision recorder is healthy")
    }

    /// The paint receipts received on the real control pipe, in order.
    /// # Panics
    /// Panics if the fixture's paint recorder is poisoned.
    pub fn shown(&self) -> Vec<(RunId, SceneId, u32)> {
        self.shown
            .lock()
            .expect("the paint recorder is healthy")
            .clone()
    }

    pub(super) async fn before_control(&self, bytes: &[u8]) {
        let mut bytes = bytes;
        let gate = match frame::read::<ControlToShell>(&mut bytes, Pipe::Control) {
            Ok(Some(ControlToShell::QueuedAudioRegistered(_))) => &self.ack,
            Ok(Some(ControlToShell::RunState {
                origin: Some(_), ..
            })) => &self.name,
            _ => return,
        };
        while !gate.load(Ordering::Acquire) {
            self.control.notified().await;
        }
    }

    pub(super) fn control(&self, message: &ControlToBrain) {
        match message {
            ControlToBrain::StepShown { run, scene, index } => {
                self.shown
                    .lock()
                    .expect("the paint recorder is healthy")
                    .push((*run, *scene, *index));
            }
            ControlToBrain::SpeechDecision(key, decision) => {
                *self
                    .decision
                    .lock()
                    .expect("the fixture decision recorder is healthy") = Some((*key, *decision));
            }
            ControlToBrain::Stop { run, .. } | ControlToBrain::StopScenes { run, .. } => {
                self.stop.store(run.0, Ordering::Release);
            }
            ControlToBrain::ClearDrawing { scene_ids } if scene_ids.len() == 1 => {
                self.clears.fetch_add(1, Ordering::Release);
            }
            ControlToBrain::SpeechStopped { run, cursor } => {
                self.speech
                    .lock()
                    .expect("the audio probe is healthy")
                    .received_quiet = Some((*run, *cursor));
            }
            ControlToBrain::SpeechRetired(receipt) => {
                self.heard.fetch_max(receipt.heard, Ordering::AcqRel);
                self.speech
                    .lock()
                    .expect("the audio probe is healthy")
                    .received_retirements
                    .push(*receipt);
            }
            ControlToBrain::SceneFailed { scene } => {
                *self
                    .scene_failed
                    .lock()
                    .expect("the fixture scene recorder is healthy") = Some((self.stop(), *scene));
            }
            _ => {}
        }
    }
}

async fn attach_hold_input(brain: &mut Brain, dir: &Path, probe: Arc<AudioProbe>) {
    let (reports, heard) = Reports::new();
    let (_, tts) = Reports::new();
    let stt = SttHandle::offline(reports, move |rate, samples| {
        assert_eq!(rate, 16_000, "the recognizer receives canonical-rate PCM");
        assert_eq!(
            samples,
            [173, -19],
            "only the released gesture reaches decode"
        );
        probe.decoded.fetch_add(1, Ordering::Release);
        "synthetic held words".into()
    });
    brain.attach_voice(
        dir.join("models"),
        &[],
        (Some(stt), None),
        [heard, tts],
        Arc::new(Offline),
    );
    let (installed, effects) = Installer::start(
        [Durable::Installed, Durable::Absent],
        [Ok(true); 2],
        [true, false],
        &[],
        true,
    );
    brain
        .voice_op(
            |current| {
                *current = installed;
                effects
            },
            None,
        )
        .expect("load the offline recognizer");
    let event = tokio::time::timeout(Duration::from_secs(3), super::voice::next(&mut brain.voice))
        .await
        .expect("offline recognizer ready");
    brain
        .voice_event(event, None)
        .expect("record offline recognizer state");
}

pub(crate) async fn brain(dir: &Path, script: &str, tone_chunks: usize) -> Brain {
    let mut store = Store::open(&dir.join("store.db")).expect("open the test store");
    store.recover().expect("recover the test store");
    let main = store.main_thread().expect("test store main thread");
    let high = store.high_water().expect("test store high water");
    let scenes = store.scenes().expect("test store scenes");
    let probe = probe();
    let narration = probe.as_ref().and_then(|probe| probe.narrated);
    let narrated = narration.is_some();
    let text_only = probe.as_ref().is_some_and(|probe| probe.text_only);
    let replay = crate::engines::responses::replay::MODEL;
    let config_source = if narrated {
        format!(
            "default_model = {{ model = \"{replay}\", effort = \"Low\", connection = \"ChatGpt\" }}\n[voice]\ntalk_back = true\n"
        )
    } else if probe.as_ref().is_some_and(|probe| probe.hold) {
        format!(
            "[voice]\ntalk_back = {}\nshow_screen = false\nhold_to_talk = true\n",
            !text_only
        )
    } else if text_only {
        "[voice]\ntalk_back = false\n".into()
    } else {
        "[voice]\ntalk_back = true\n".into()
    };
    std::fs::write(dir.join(CONFIG_FILE), config_source)
        .expect("configure Talk Back in this fixture");
    let config = Config::load(Some(dir.to_path_buf()), main);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let lock = dir.join(SESSION_LOCK);
    let worker = Worker::new(
        Box::new(move || Vault::with_service(Offline, "test-accounts", &lock)),
        Arc::clone(&clock),
    );
    let accounts = Accounts::spawn_unconfigured(
        worker,
        Net {
            http: Arc::new(|| Err(HttpError::Configuration)),
            launcher: Arc::new(Offline),
            clock,
        },
    )
    .expect("start offline accounts");
    let engine = if narrated {
        Selected::DrawReplay(Box::new(
            if let Some(turns) = probe.as_ref().and_then(|probe| probe.changing_turns) {
                crate::engines::responses::replay::Replay::changing_turns(turns)
            } else if let Some(hold) = probe
                .as_ref()
                .and_then(|probe| probe.replacement_chain.clone())
            {
                Ok(crate::engines::responses::replay::Replay::replacement_chain(hold))
            } else if probe
                .as_ref()
                .is_some_and(|probe| probe.initial_credit.is_some())
            {
                crate::engines::responses::replay::Replay::replacing()
            } else {
                crate::engines::responses::replay::Replay::narrated()
            }
            .expect("read the narrated fixture"),
        ))
    } else {
        Selected::Fixture(Box::new(
            Fixture::parse(script).expect("read the answer fixture"),
        ))
    };
    let mut brain = Brain::new(store, main, high, scenes, engine, config, accounts)
        .expect("start the fixture Brain");
    if let Some(probe) = probe.filter(|probe| probe.hold) {
        attach_hold_input(&mut brain, dir, probe).await;
    }
    if text_only {
        return brain;
    }
    let (reports, mut loaded) = Reports::new();
    let tts = TtsHandle::spawn_with(tokio::runtime::Handle::current(), reports, move |_| {
        Ok(Tone(tone_chunks))
    })
    .expect("start fixture synthesis");
    tts.load(dir.to_path_buf(), 1);
    let ready = tokio::time::timeout(Duration::from_secs(3), loaded.recv())
        .await
        .expect("fixture synthesis started before the deadline")
        .expect("fixture synthesis reported");
    assert_eq!(
        (ready.ask, ready.event),
        (1, LoadEvent::Loaded),
        "the fake model must be ready"
    );
    brain.pump = Some(Pump::new(tts, narration.unwrap_or(OutputMode::Legacy)));
    brain.offline(Input::SpokenReplies { loaded: true });
    brain
}
