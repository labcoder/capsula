//! A scripted render device for tests: the real worker, engine and serve loop run against it.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::{Device, Engine, Fault, Observation, Outlet, Release, Speaker, launch, serve};

/// The device's rate, and its clock's units per second.
pub(crate) const RATE: u32 = 24_000;
/// Frames each pass writes.
const FRAMES: usize = 240;

/// What the test scripts and what the device saw.
#[derive(Debug, Default)]
#[expect(clippy::struct_excessive_bools, reason = "each is one scripted switch")]
pub(crate) struct Script {
    /// The clock reports this many frames played, at most all but the last frame written.
    pub(crate) play_to: u64,
    /// The worker waits at its next pass until cleared; `parked` says it's waiting.
    pub(crate) frozen: bool,
    pub(crate) parked: bool,
    /// A release holding content panics.
    pub(crate) panic: bool,
    pub(crate) stop_fails: bool,
    /// Each pass fails while set.
    pub(crate) fail: bool,
    pub(crate) stopped: bool,
    /// Every nonzero sample released as content.
    pub(crate) content: Vec<i16>,
    /// Frames released as silence.
    pub(crate) silent: u64,
    /// Passes the device was serviced.
    pub(crate) passes: u64,
}

pub(crate) type Shared = Arc<Mutex<Script>>;

pub(crate) fn lock(script: &Shared) -> MutexGuard<'_, Script> {
    script.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Fake {
    script: Shared,
    written: u64,
    host: u64,
}

impl Device for Fake {
    fn wait(&self) -> Result<(), Fault> {
        loop {
            let mut script = lock(&self.script);
            script.parked = script.frozen;
            if !script.frozen {
                break;
            }
            drop(script);
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(1));
        Ok(())
    }

    #[expect(
        clippy::panic_in_result_fn,
        reason = "the scripted panic is the fault under test"
    )]
    fn service(&mut self, engine: &mut Engine) -> Result<(), Fault> {
        let (played, panic) = {
            let mut script = lock(&self.script);
            script.passes += 1;
            if script.fail {
                return Err(Fault::Lost);
            }
            (
                script.play_to.min(self.written.saturating_sub(1)),
                script.panic,
            )
        };
        self.host += 1;
        engine.observe(Observation {
            position: played,
            frequency: u64::from(RATE),
            host_time: self.host,
            host_frequency: 1_000,
            precise: true,
            service: Duration::ZERO,
        })?;
        let unread = u32::try_from(self.written - played).expect("a few frames");
        let frames = u32::try_from(FRAMES).expect("a pass");
        let mut out = [0; FRAMES];
        let staged = engine.stage(&mut out, unread)?;
        staged.release(
            |release| {
                let content = out.iter().filter(|sample| **sample != 0);
                if release == Release::Content && content.clone().next().is_some() {
                    assert!(!panic, "the scripted panic, mid-release");
                    lock(&self.script).content.extend(content);
                }
                if release == Release::Silent {
                    lock(&self.script).silent += u64::from(frames);
                }
                Ok(Duration::ZERO)
            },
            || Some(unread + frames),
        )?;
        self.written += u64::from(frames);
        Ok(())
    }

    fn stop(&mut self) -> bool {
        let mut script = lock(&self.script);
        script.stopped = true;
        !script.stop_fails
    }
}

/// A speaker on the scripted device, launched as a platform's would be.
pub(crate) fn open(outlet: Outlet, script: Shared) -> Option<Speaker> {
    launch(outlet, Duration::from_secs(5), move |worker, ready| {
        let mut engine = Engine::new(
            RATE,
            worker.generation(),
            10_000_000,
            worker.revocation(),
            worker.tail(),
        );
        let mut device = Fake {
            script,
            written: 0,
            host: 0,
        };
        let started =
            engine.begin(u64::from(RATE), 0).is_ok() && device.service(&mut engine).is_ok();
        let _told = ready.send(started);
        if started {
            serve(worker, engine, device);
        }
    })
}
