use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kumochi_protocol::constants::{CONFIG_FILE, SESSION_LOCK};
use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    ControlToBrain, ControlToShell, Cursor, RunId, SceneId, SpeechRetirement,
};
use tokio::sync::Notify;

use crate::connections::account::Worker;
use crate::connections::clock::{Clock, SystemClock};
use crate::connections::http_client::HttpError;
use crate::connections::oauth::{LaunchError, LaunchTarget, Launcher};
use crate::connections::vault::Vault;
use crate::engines::fixture::Fixture;
use crate::platform::credential::{CredentialStore, CredentialStoreError, SecretBytes};
use crate::voice::pump::{OutputMode, Pump};
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

struct Offline;

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
    ack: AtomicBool,
    name: AtomicBool,
    control: Notify,
    pub(super) data_blocked: AtomicBool,
    apply_credit: AtomicBool,
    received_credit: AtomicU64,
    applied_credit: AtomicU64,
    stop: AtomicU64,
    heard: AtomicU32,
    speech: Mutex<SpeechEvidence>,
    scene_failed: std::sync::Mutex<Option<(RunId, SceneId)>>,
}

#[derive(Debug, Default)]
struct SpeechEvidence {
    retired: Vec<SpeechRetirement>,
    received_retirements: Vec<SpeechRetirement>,
    received_quiet: Option<(RunId, Option<Cursor>)>,
    accepted_quiet: Option<(RunId, Option<Cursor>)>,
}

impl AudioProbe {
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

    /// Applies subsequent wire grants to the real Brain data queue.
    pub fn allow_data_credit(&self) {
        self.apply_credit.store(true, Ordering::Release);
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
            ControlToBrain::Stop { run, .. } | ControlToBrain::StopScenes { run, .. } => {
                self.stop.store(run.0, Ordering::Release);
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

pub(crate) async fn brain(dir: &Path, script: &str, tone_chunks: usize) -> Brain {
    let mut store = Store::open(&dir.join("store.db")).expect("open the test store");
    store.recover().expect("recover the test store");
    let main = store.main_thread().expect("test store main thread");
    let high = store.high_water().expect("test store high water");
    let scenes = store.scenes().expect("test store scenes");
    std::fs::write(dir.join(CONFIG_FILE), "[voice]\ntalk_back = true\n")
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
    let fixture = Fixture::parse(script).expect("read the answer fixture");
    let mut brain = Brain::new(
        store,
        main,
        high,
        scenes,
        Selected::Fixture(Box::new(fixture)),
        config,
        accounts,
    )
    .expect("start the fixture Brain");
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
    brain.pump = Some(Pump::new(tts, OutputMode::Legacy));
    brain.offline(Input::SpokenReplies { loaded: true });
    brain
}
