//! A spoken run's speech, wired to the run: the run's [`Reply`] decides, and this module carries
//! its [`Speech`] out as effects and routes the shell's and the pump's reports back to it.
//!
//! - **One completion predicate.** [`Coordinator::complete`] is the only place a run ends
//!   `Done`: the round finished, nothing is outstanding, the drawing settled, and the reply is
//!   final. Until then the run waits with `finishing` set, which is Draining; the core loop's
//!   settle pass re-checks it after every input.
//! - **One revoke** on every other ending, and a terminal that waits for both the store and the
//!   shell's quiet answer.

use std::fmt;
use std::time::Duration;

use kumochi_protocol::message::{ControlToShell, DataToShell, OutputGeneration, RunId, SegmentId};

use super::{Coordinator, Effect, Input, Notify, Run, Stale};
use crate::store::{Interruption, RunEnd};
use crate::voice::reply::{Failure, Reply, Revoker, Speech};

/// How long a draining run may go without progress before its speech is revoked [guess].
pub const DRAIN_STALL: Duration = Duration::from_secs(5);
/// How long the shell has to go quiet after a revoke before its connection ends [guess].
pub const QUIET_TIMEOUT: Duration = Duration::from_secs(2);

/// What TTS made of a segment, as the pump forwards it. Its Debug shows only lengths.
#[derive(Clone, PartialEq, Eq)]
pub enum Synthesized {
    /// Audio at the output rate.
    Audio(Vec<i16>),
    /// The segment is finished.
    Done,
    /// The segment failed; with `run`, TTS can't speak the rest of the run either.
    Failed {
        /// The failure covers the whole run.
        run: bool,
    },
}

impl fmt::Debug for Synthesized {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Audio(samples) => f
                .debug_struct("Audio")
                .field("samples", &samples.len())
                .finish(),
            Self::Done => f.write_str("Done"),
            Self::Failed { run } => f.debug_struct("Failed").field("run", run).finish(),
        }
    }
}

impl Coordinator {
    /// The run ends `Done` once its speech is final; until then it drains.
    pub(super) fn complete(&mut self, run: RunId, effects: &mut Vec<Effect>) -> Result<(), Stale> {
        self.current(run)?.finishing = true;
        self.fence_mcp(effects);
        let current = self.current(run)?;
        if let Some(reply) = &mut current.reply {
            let mut out = Vec::new();
            reply.close(&mut out);
            carry(current, out, effects);
            if !reply_final(current) {
                return Ok(());
            }
        }
        let done = self.take(run)?;
        self.end(done, RunEnd::Done, None, Notify::Shell, effects);
        Ok(())
    }

    pub(super) fn reserve_say(
        current: &mut Run,
        say: &str,
        effects: &mut Vec<Effect>,
    ) -> Option<SegmentId> {
        let mut out = Vec::new();
        let reserved = current.reply.as_mut()?.reserve_say(say, &mut out);
        carry(current, out, effects);
        reserved
    }

    /// A scene failed: its say segments are dropped from the run that drew it.
    pub(super) fn step_failed(
        &mut self,
        run: RunId,
        scene: kumochi_protocol::message::SceneId,
        effects: &mut Vec<Effect>,
    ) {
        if let Ok(found) = self.find(run)
            && let Some(reply) = &mut found.reply
        {
            let mut out = Vec::new();
            reply.step_failed(scene, &mut out);
            carry(found, out, effects);
        }
    }

    /// The text of the engine's delta joins its run's speech.
    pub(super) fn spoken_text(&mut self, run: RunId, delta: &str, effects: &mut Vec<Effect>) {
        if let Ok(current) = self.current(run)
            && let Some(reply) = &mut current.reply
        {
            let mut out = Vec::new();
            reply.text(delta, &mut out);
            carry(current, out, effects);
        }
    }

    /// The shell's output device; one at or below a withdrawn generation stays withdrawn.
    fn announce(&mut self, rate: u32, generation: OutputGeneration, effects: &mut Vec<Effect>) {
        if self
            .withdrawn
            .is_some_and(|withdrawn| generation.0 <= withdrawn.0)
        {
            return;
        }
        self.output = Some((rate, generation));
        if let Some(current) = self.current.as_mut()
            && let Some(reply) = &mut current.reply
        {
            let mut out = Vec::new();
            reply.output(generation, &mut out);
            carry(current, out, effects);
        }
    }

    pub(super) fn speech_input(
        &mut self,
        input: Input,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let run = match input {
            Input::AudioOutput { rate, generation } => {
                self.announce(rate, generation, effects);
                return Ok(());
            }
            Input::AudioOutputUnavailable { generation } => {
                if self
                    .output
                    .is_some_and(|(_, current)| current == generation)
                {
                    self.output = None;
                    self.withdrawn = Some(generation);
                    if let Some(current) = self.current.as_mut()
                        && let Some(reply) = &mut current.reply
                    {
                        let mut out = Vec::new();
                        reply.withdrawn(generation, &mut out);
                        carry(current, out, effects);
                    }
                }
                return Ok(());
            }
            Input::SpokenReplies { loaded } => {
                self.spoken = loaded;
                return Ok(());
            }
            Input::DrainTimeout { run, epoch } => {
                let current = self.current(run)?;
                if !current
                    .reply
                    .as_ref()
                    .is_some_and(|reply| reply.stalled(epoch))
                {
                    return Err(Stale);
                }
                let stalled = self.take(run)?;
                let stopped = RunEnd::Interrupted(Interruption::Stop);
                self.end(stalled, stopped, None, Notify::Shell, effects);
                return Ok(());
            }
            Input::QuietTimeout { run } => {
                let shell = self
                    .find(run)?
                    .ending
                    .as_ref()
                    .is_none_or(|ending| ending.notify == Notify::Shell);
                let reply = self.find(run)?.reply.as_ref().ok_or(Stale)?;
                if !shell || !reply.revoked() || reply.is_final() {
                    return Err(Stale);
                }
                effects.push(Effect::Broken("speech never went quiet"));
                return Ok(());
            }
            Input::Tts { run, .. }
            | Input::SpeechProgress { run, .. }
            | Input::SegmentPlayed { run, .. }
            | Input::SpeechStopped { run, .. } => run,
            _ => unreachable!("only speech inputs are routed here"),
        };
        let found = self.find(run)?;
        let reply = found.reply.as_mut().ok_or(Stale)?;
        let mut out = Vec::new();
        let reported = match input {
            Input::Tts { segment, made, .. } => {
                match made {
                    Synthesized::Audio(samples) => reply.pcm(segment, samples, &mut out),
                    Synthesized::Done => reply.done(segment, &mut out),
                    Synthesized::Failed { run } => {
                        let failure = if run { Failure::Run } else { Failure::Segment };
                        reply.failed(segment, failure, &mut out);
                    }
                }
                Ok(())
            }
            Input::SpeechProgress {
                segment, sample, ..
            } => reply.progress(segment, sample, &mut out),
            Input::SegmentPlayed {
                segment, samples, ..
            } => reply.played(segment, samples, &mut out),
            Input::SpeechStopped { cursor, .. } => {
                // A second answer, for a Stop that crossed a revoke, changes nothing.
                if !reply.revoked() || reply.is_final() {
                    return Err(Stale);
                }
                reply.stopped(cursor)
            }
            _ => unreachable!("matched above"),
        };
        carry(found, out, effects);
        if reported.is_err() {
            effects.push(Effect::Broken("a speech report that can't be true"));
        }
        self.release(run, effects);
        Ok(())
    }
}

/// Stops a run's speech for good, when it has any.
pub(super) fn revoke(run: &mut Run, by: Revoker, effects: &mut Vec<Effect>) {
    if let Some(reply) = &mut run.reply {
        let mut out = Vec::new();
        reply.revoke(by, &mut out);
        carry(run, out, effects);
    }
}

fn reply_final(run: &Run) -> bool {
    run.reply.as_ref().is_none_or(Reply::is_final)
}

/// Carries out a run's speech. Frames for the shell wait behind a pending step like the run's
/// text, so the shell sees them in the engine's order.
pub(super) fn carry(run: &mut Run, out: Vec<Speech>, effects: &mut Vec<Effect>) {
    let id = run.id;
    let rate = run.reply.as_ref().map_or(0, Reply::rate);
    for speech in out {
        let effect = match speech {
            Speech::Queued {
                segment,
                step,
                start,
                output_generation,
            } => Effect::Data(DataToShell::SegmentQueued {
                run: id,
                segment,
                step,
                start,
                output_generation,
            }),
            Speech::Pcm(pcm) => Effect::Data(DataToShell::SpeechPcm(pcm)),
            Speech::Speak { segment, text } => Effect::Speak {
                run: id,
                segment,
                rate,
                text: text.into(),
            },
            Speech::Credit(samples) => Effect::Credit { run: id, samples },
            Speech::Cancel => Effect::CancelSpeech { run: id },
            Speech::Revoke { tell_shell } => {
                effects.push(Effect::RevokeSpeech { run: id });
                if tell_shell {
                    effects.push(Effect::Control(ControlToShell::SpeechRevoked { run: id }));
                }
                Effect::ArmQuiet {
                    run: id,
                    after: QUIET_TIMEOUT,
                }
            }
            Speech::Arm { epoch } => Effect::ArmDrain {
                run: id,
                epoch,
                after: DRAIN_STALL,
            },
        };
        match (effect, run.held.back_mut()) {
            (Effect::Data(frame), Some(behind)) => behind.push(frame),
            (effect, _) => effects.push(effect),
        }
    }
}
