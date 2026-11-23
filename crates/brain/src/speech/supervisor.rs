use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kumochi_protocol::message::MAX_TRANSCRIPT_BYTES;
use tokio::sync::{broadcast, watch};
use tokio::time::Instant;

use super::wire::{
    MAX_PCM_BYTES, NativeError, SpeechError, SpeechReply, SpeechRequest, check_audio,
};
use crate::platform::Job;
use crate::supervise::{self, Completion, Dispatch, Exit, Generation, Handle, Kind, Retirement};

/// The speech worker's kind for the shared supervisor.
pub(super) struct Speech;

impl Kind for Speech {
    const ARG: &'static str = crate::SPEECH_WORKER;
    const NAME: &'static str = "speech";
    /// The PCM ceiling plus room for two paths and the encoding.
    const MAX_REQUEST: usize = MAX_PCM_BYTES + 256 * 1024;
    /// A transcript and its encoding.
    const MAX_REPLY: usize = MAX_TRANSCRIPT_BYTES + 1024;
    type Request = SpeechRequest;
    type Reply = SpeechReply;
    type Assembly = crate::supervise::Assembly;
}

/// Each request's absolute budget, from the call through queueing, a respawn, chunking and the
/// native work.
#[derive(Clone, Copy, Debug)]
pub struct Deadlines {
    /// Staging, loading and the warm-up.
    pub load: Duration,
    /// A decode's fixed part.
    pub decode: Duration,
    /// A decode's share of the audio's length, in thousandths.
    pub decode_per_mille: u32,
    /// Freeing the model and the natives.
    pub unload: Duration,
}

/// Load 30 s; decode 2 s plus a quarter of the audio's length; unload 5 s [guess]. A 30 s clip
/// decoded in 1.9 s on Rez (CAPS-157, run 2).
pub const DEADLINES: Deadlines = Deadlines {
    load: Duration::from_secs(30),
    decode: Duration::from_secs(2),
    decode_per_mille: 250,
    unload: Duration::from_secs(5),
};

/// The speech worker's lifecycle owner.
#[derive(Debug)]
pub struct SpeechSupervisor;

impl SpeechSupervisor {
    /// Starts the worker directly in the brain job, with nothing loaded.
    ///
    /// # Errors
    /// Returns a process or thread creation failure.
    pub fn start(exe: PathBuf, jobs: Arc<Job>) -> io::Result<SpeechHandle> {
        Self::with_deadlines(exe, jobs, DEADLINES)
    }

    /// Starts with injected budgets so fixtures don't race the product's.
    ///
    /// # Errors
    /// Returns a process or thread creation failure.
    pub fn with_deadlines(
        exe: PathBuf,
        jobs: Arc<Job>,
        deadlines: Deadlines,
    ) -> io::Result<SpeechHandle> {
        Ok(SpeechHandle {
            inner: supervise::start::<Speech>(exe, jobs)?,
            deadlines,
        })
    }
}

/// Requests and loss notifications; dropping the last handle stops the worker. A replacement has
/// nothing loaded until the next [`SpeechHandle::load`]; a request failure alone proves no release.
#[derive(Clone, Debug)]
pub struct SpeechHandle {
    inner: Handle<Speech>,
    deadlines: Deadlines,
}

impl SpeechHandle {
    /// Stages the natives from `natives` and loads Parakeet from `models`, replacing any model.
    /// Returns the generation of the worker that holds the model, which its loss names.
    ///
    /// # Errors
    /// Returns `WorkerLost`, `ExitUnconfirmed`, or the native failure.
    pub async fn load(&self, natives: PathBuf, models: PathBuf) -> Result<Generation, SpeechError> {
        let until = Instant::now() + self.deadlines.load;
        loaded(
            self.inner
                .request_resource(SpeechRequest::Load { natives, models }, until, None, None)
                .await
                .map_err(SpeechError::from)?,
        )
    }

    pub(crate) async fn load_tracked(
        &self,
        natives: PathBuf,
        models: PathBuf,
        dispatch: watch::Sender<Dispatch>,
    ) -> Result<Generation, SpeechError> {
        let until = Instant::now() + self.deadlines.load;
        loaded(
            self.inner
                .request_resource(
                    SpeechRequest::Load { natives, models },
                    until,
                    None,
                    Some(dispatch),
                )
                .await
                .map_err(SpeechError::from)?,
        )
    }

    /// Transcribes mono PCM at its capture rate. The audio is checked before anything is sent.
    ///
    /// # Errors
    /// Returns `Input`, `WorkerLost`, `ExitUnconfirmed`, `NotLoaded`, or the native failure.
    pub async fn decode(&self, rate: u32, samples: &[i16]) -> Result<String, SpeechError> {
        check_audio(rate, samples.len())?;
        let samples_ms = u64::try_from(samples.len()).map_err(|_| SpeechError::Input)? * 1000;
        let audio = Duration::from_millis(samples_ms / u64::from(rate));
        let until =
            Instant::now() + self.deadlines.decode + audio * self.deadlines.decode_per_mille / 1000;
        let pcm = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        match self
            .send(SpeechRequest::Decode { rate, pcm }, until)
            .await?
            .0
        {
            SpeechReply::Text(text) if text.len() <= MAX_TRANSCRIPT_BYTES => Ok(text),
            SpeechReply::Text(_) => Err(SpeechError::Native(NativeError::Text)),
            other => Err(refusal(&other)),
        }
    }

    /// Drops the model and the natives in the worker.
    ///
    /// # Errors
    /// A failure does not prove the model has been freed.
    pub async fn unload(&self) -> Result<(), SpeechError> {
        self.unload_generation().await.map(drop)
    }

    pub(crate) async fn unload_generation(&self) -> Result<Generation, SpeechError> {
        let until = Instant::now() + self.deadlines.unload;
        match self.send(SpeechRequest::Unload, until).await? {
            (SpeechReply::Unloaded, generation) => Ok(generation),
            (other, _) => Err(refusal(&other)),
        }
    }

    pub(crate) fn unload_owned(
        &self,
        generation: Generation,
    ) -> impl std::future::Future<Output = Result<Generation, SpeechError>> + '_ {
        let until = Instant::now() + self.deadlines.unload;
        let request =
            self.inner
                .request_resource(SpeechRequest::Unload, until, Some(generation), None);
        async move {
            let reply = request.await.map_err(SpeechError::from)?;
            if !matches!(reply.reply(), SpeechReply::Unloaded) {
                return Err(refusal(reply.reply()));
            }
            reply
                .handoff()
                .map(|(_, generation)| generation)
                .map_err(SpeechError::from)
        }
    }

    /// Raises one failure in the worker, under the decode budget.
    ///
    /// # Errors
    /// Returns `WorkerLost`, which every fixture should cause.
    #[cfg(debug_assertions)]
    pub async fn fault(&self, fault: super::SpeechFault) -> Result<(), SpeechError> {
        let until = Instant::now() + self.deadlines.decode;
        self.send(SpeechRequest::Fault(fault), until)
            .await
            .map(drop)
    }

    /// Subscribes before issuing work, so a lost model can't be missed. Each loss names the
    /// generation that ended.
    pub fn lost(&self) -> broadcast::Receiver<Generation> {
        self.inner.lost()
    }

    pub(crate) fn exits(&self) -> broadcast::Receiver<Exit> {
        self.inner.exits()
    }

    pub(crate) fn retirement(&self) -> Retirement {
        self.inner.retirement()
    }

    async fn send(
        &self,
        request: SpeechRequest,
        until: Instant,
    ) -> Result<(SpeechReply, Generation), SpeechError> {
        self.inner
            .request(request, until)
            .await
            .map_err(SpeechError::from)
    }
}

fn loaded(reply: Completion<SpeechReply>) -> Result<Generation, SpeechError> {
    let result = match reply.reply() {
        SpeechReply::Loaded => Ok(reply.generation()),
        SpeechReply::Failed(error) => Err(SpeechError::Native(*error)),
        other => return Err(refusal(other)),
    };
    reply.handoff().map_err(SpeechError::from)?;
    result
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;

/// A reply that refuses the request. One that answers a different request breaks the worker's
/// contract, which the caller sees as a loss.
fn refusal(reply: &SpeechReply) -> SpeechError {
    match reply {
        SpeechReply::Failed(error) => SpeechError::Native(*error),
        SpeechReply::NotLoaded => SpeechError::NotLoaded,
        SpeechReply::Loaded | SpeechReply::Text(_) | SpeechReply::Unloaded => {
            SpeechError::WorkerLost
        }
    }
}
