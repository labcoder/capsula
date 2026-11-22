//! Provider draw ownership, bounded argument verification and independent result settlement.

use std::fmt;

use kumochi_protocol::message::RunId;
use kumochi_scene::{TokenError, Tokenizer};
use sha2::{Digest as _, Sha256};

use super::{BuildKey, Coordinator, Effect, Stale};
use crate::dispatch::{CallInput, Dispatch, DrawAction, DrawEnd, LedgerKey, ToolOutput};
use crate::drawing::{CallEnd, CallError, Input};
use crate::engines::{CallId, ToolName};

pub(super) struct DrawCall {
    pub(super) call: CallId,
    pub(super) key: LedgerKey,
    pub(super) build: Option<BuildKey>,
    pub(super) provider: Option<ProviderCall>,
}

pub(super) struct ProviderCall {
    identity: Identity,
    stopped: bool,
    finished: bool,
    result: Option<CallEnd>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Identity {
    Pending,
    Canonical,
    Failed,
}

pub(super) struct ProviderDraw {
    pub(super) build: BuildKey,
    raw: Vec<u8>,
    tokenizer: Tokenizer,
    owner: Option<usize>,
    ready: bool,
    pub(super) registered: bool,
    /// Text followed the closed records array; it is never read.
    after_records: bool,
}

impl fmt::Debug for DrawCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DrawCall")
            .field("provider", &self.provider.is_some())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for ProviderDraw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderDraw")
            .field("bytes", &self.raw.len())
            .field("tokenizer", &self.tokenizer)
            .finish_non_exhaustive()
    }
}

impl DrawCall {
    pub(super) fn ended(&mut self, end: CallEnd) -> Option<(LedgerKey, DrawEnd)> {
        if let Some(provider) = &mut self.provider {
            provider.stopped = true;
            provider.result = Some(end);
            self.result()
        } else {
            Some((self.key.clone(), draw_end(&end)))
        }
    }

    fn result(&mut self) -> Option<(LedgerKey, DrawEnd)> {
        let provider = self.provider.as_mut()?;
        if provider.identity == Identity::Pending {
            return None;
        }
        let result = provider.result.take()?;
        Some((self.key.clone(), draw_end(&result)))
    }
}

/// A finished draw call's result, which stopped early when it carries an error.
fn draw_end(end: &CallEnd) -> DrawEnd {
    DrawEnd {
        output: ToolOutput::Text(end.tool_text()),
        stopped: end.error.is_some(),
    }
}

impl super::Run {
    fn provider_draw(&mut self, call: &CallId) -> Result<&mut ProviderDraw, Stale> {
        self.tools
            .open
            .iter_mut()
            .find(|open| open.call == *call)
            .and_then(|open| open.provider.as_mut())
            .ok_or(Stale)
    }
}

impl Coordinator {
    pub(super) fn provider_sources_changed(&mut self, effects: &mut Vec<Effect>) {
        let affected = self.current.as_ref().and_then(|run| {
            if !run
                .tools
                .open
                .iter()
                .any(|open| open.provider.as_ref().is_some_and(|p| p.owner.is_some()))
            {
                return None;
            }
            let build = run.build.as_ref().filter(|build| build.admitted())?;
            let voided = build.voided(build.sources());
            (!voided.is_empty()).then_some((build.key(), voided))
        });
        if let Some((key, voided)) = affected {
            effects.push(Effect::CancelEngine { run: key.run });
            self.rebuild(key, &voided, &self.pending_deletes(), effects);
        }
    }

    pub(super) fn provider_started(
        &mut self,
        build: BuildKey,
        call: &CallId,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let run = self.current(build.run)?;
        if !run.tools.open(call.clone(), ToolName::Draw) {
            return self.fail(build.run, crate::store::Failure::Engine, effects);
        }
        let caller = run.model_caller(call.clone());
        if !run.provider_calls.contains(&caller.key()) {
            run.provider_calls.push(caller.key());
        }
        let before = run.draw_calls.len();
        let dispatch = self.tool_input(
            caller,
            ToolName::Draw,
            CallInput::Draw(DrawAction::Start),
            effects,
        );
        let Some(run) = self.current.as_mut().filter(|run| {
            run.id == build.run
                && run
                    .build
                    .as_ref()
                    .is_some_and(|held| held.key() == build && held.admitted())
        }) else {
            return Ok(());
        };
        let owner = (run.draw_calls.len() > before).then_some(before);
        if let Some(at) = owner {
            run.draw_calls[at].provider = Some(ProviderCall {
                identity: Identity::Pending,
                stopped: false,
                finished: false,
                result: None,
            });
        }
        let registered = owner.is_some() || matches!(dispatch, Dispatch::Answered(_));
        let open = run
            .tools
            .open
            .iter_mut()
            .find(|open| open.call == *call)
            .expect("the call was opened");
        open.provider = Some(ProviderDraw {
            build,
            raw: Vec::with_capacity(crate::dispatch::MAX_ARGS),
            tokenizer: Tokenizer::new(),
            owner,
            ready: false,
            registered,
            after_records: false,
        });
        if let Dispatch::Answered(output) = dispatch {
            run.tools.answer(call, output);
        }
        Ok(())
    }

    pub(super) fn is_provider(&self, run: RunId, call: &CallId) -> bool {
        self.current
            .as_ref()
            .filter(|current| current.id == run)
            .is_some_and(|current| {
                current
                    .tools
                    .open
                    .iter()
                    .any(|open| open.call == *call && open.provider.is_some())
            })
    }

    pub(super) fn provider_delta<const REPLAY: bool>(
        &mut self,
        run: RunId,
        call: &CallId,
        delta: &str,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let accepted = {
            let current = self.current(run)?;
            let p = current.provider_draw(call)?;
            if p.ready {
                return self.fail(run, crate::store::Failure::Engine, effects);
            }
            let take = delta.len().min(crate::dispatch::MAX_ARGS - p.raw.len());
            p.raw.extend_from_slice(&delta.as_bytes()[..take]);
            take
        };
        let mut rest = &delta.as_bytes()[..accepted];
        loop {
            let next = {
                let current = self.current(run)?;
                let p = current.provider_draw(call)?;
                p.tokenizer.next(&mut rest).map(|record| {
                    record.map(|record| {
                        #[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
                        if REPLAY {
                            mark_close(run, record.index, record.bytes.len());
                        }
                        (record.index, record.bytes.to_vec())
                    })
                })
            };
            let (index, bytes) = match next {
                Ok(Some(record)) => record,
                Ok(None) => break,
                Err(TokenError::AfterRecords) => {
                    self.current(run)?.provider_draw(call)?.after_records = true;
                    break;
                }
                Err(error) => {
                    self.provider_invalid(run, call, CallError::Token(error), effects)?;
                    return Ok(());
                }
            };
            if usize::try_from(index).map_or(true, |index| {
                index >= kumochi_protocol::message::MAX_DRAW_STEPS_PER_RUN + 2
            }) {
                self.provider_invalid(run, call, CallError::Exhausted, effects)?;
                return Ok(());
            }
            if let Err(error) = crate::dispatch::validate_draw_record(&bytes) {
                self.provider_invalid(run, call, error, effects)?;
                return Ok(());
            }
            let handoff = {
                let current = self.current(run)?;
                let p = current.provider_draw(call)?;
                let owner = p.owner;
                owner.is_some_and(|at| {
                    current.draw_calls[at]
                        .provider
                        .as_ref()
                        .is_some_and(|owner| !owner.stopped && !owner.finished)
                })
            };
            if handoff {
                let caller = self.current(run)?.model_caller(call.clone());
                if let Dispatch::Answered(_) = self.tool_input(
                    caller,
                    ToolName::Draw,
                    CallInput::Draw(DrawAction::Record { index, bytes }),
                    effects,
                ) {
                    return self.fail(run, crate::store::Failure::Engine, effects);
                }
                if !self.is_provider(run, call) {
                    return Ok(());
                }
            }
        }
        if accepted != delta.len() {
            self.provider_invalid(
                run,
                call,
                CallError::Token(TokenError::DrawingTooLarge),
                effects,
            )?;
        }
        Ok(())
    }

    pub(super) fn provider_ready(
        &mut self,
        run: RunId,
        call: &CallId,
        args: &str,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let (owner, validation) = {
            let current = self.current(run)?;
            let p = current.provider_draw(call)?;
            if p.ready {
                return self.fail(run, crate::store::Failure::Engine, effects);
            }
            p.ready = true;
            let validation = if p.raw == args.as_bytes() {
                match p.tokenizer.finish() {
                    Ok(()) | Err(TokenError::AfterRecords) => Ok(()),
                    Err(error) => Err(CallError::Token(error)),
                }
                .and_then(|()| {
                    crate::dispatch::validate_draw(args.as_bytes()).map(|draw| draw.hash)
                })
            } else {
                Err(CallError::Token(TokenError::Malformed { at: p.raw.len() }))
            };
            (p.owner, validation)
        };
        self.current(run)?.tools.ready(call, String::new());
        match validation {
            Err(error) => self.provider_invalid(run, call, error, effects)?,
            Ok(hash) => {
                if let Some(at) = owner {
                    let current = self.current(run)?;
                    let association = &mut current.draw_calls[at];
                    let provider = association
                        .provider
                        .as_mut()
                        .expect("the stream owns the association");
                    if provider.identity == Identity::Pending {
                        provider.identity = Identity::Canonical;
                        current.ledger.bind_stream(&association.key, hash, true);
                    }
                    self.provider_result(run, at, effects)?;
                } else {
                    self.current(run)?
                        .tools
                        .open
                        .iter_mut()
                        .find(|open| open.call == *call)
                        .and_then(|open| open.provider.as_mut())
                        .ok_or(Stale)?
                        .registered = true;
                    let caller = self.current(run)?.model_caller(call.clone());
                    let answer = self.tool_call(caller, ToolName::Draw, args.as_bytes(), effects);
                    if let Dispatch::Answered(output) = answer {
                        self.current(run)?.tools.answer(call, output);
                    }
                }
            }
        }
        Ok(())
    }

    fn provider_invalid(
        &mut self,
        run: RunId,
        call: &CallId,
        error: CallError,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let owner = {
            let current = self.current(run)?;
            let p = current
                .tools
                .open
                .iter()
                .find(|open| open.call == *call)
                .and_then(|open| open.provider.as_ref())
                .ok_or(Stale)?;
            let kind: &[u8] = match &error {
                CallError::Step(_) => b"typed",
                CallError::Token(_) => b"syntax",
                CallError::Exhausted => b"limit",
                CallError::Interrupted => b"interrupted",
                CallError::Store => b"store",
                CallError::Failed => b"preparation",
                CallError::TooMuchToPaint { .. } => b"paint",
                CallError::NoScreenPicture
                | CallError::OtherDisplay
                | CallError::ScreenOff
                | CallError::ScreenChanged => b"screen",
            };
            let hash: [u8; 32] = Sha256::new()
                .chain_update(b"draw-prefix-failure\0")
                .chain_update(kind)
                .chain_update(&p.raw)
                .finalize()
                .into();
            p.owner.map(|at| (at, hash))
        };
        let Some((at, hash)) = owner else {
            if self.current(run)?.provider_draw(call)?.registered {
                return Ok(());
            }
            self.current(run)?.provider_draw(call)?.registered = true;
            let caller = self.current(run)?.model_caller(call.clone());
            let dispatch = self.tool_call(caller, ToolName::Draw, b"", effects);
            if !self.is_provider(run, call) {
                return Ok(());
            }
            let current = self.current(run)?;
            if let Dispatch::Answered(output) = dispatch {
                current.tools.answer(call, output);
            }
            return Ok(());
        };
        let (key, id, stopped) = {
            let current = self.current(run)?;
            let association = &mut current.draw_calls[at];
            let provider = association
                .provider
                .as_mut()
                .expect("the stream owns the association");
            if provider.identity != Identity::Pending {
                return Ok(());
            }
            provider.identity = Identity::Failed;
            provider.finished = true;
            (
                association.key.clone(),
                association.call.clone(),
                provider.stopped,
            )
        };
        self.current(run)?.ledger.bind_stream(&key, hash, false);
        if !stopped {
            let thread = self.current(run)?.thread;
            let output = self.drawing.handle(Input::CallFinished {
                thread,
                run,
                call: id,
                error: Some(error),
            })?;
            self.drawing_output(output, effects);
        }
        self.provider_result(run, at, effects)
    }

    fn provider_result(
        &mut self,
        run: RunId,
        at: usize,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        if let Some((key, output)) = self.current(run)?.draw_calls[at].result() {
            self.drawn(&key, output, effects);
        }
        Ok(())
    }

    pub(super) fn provider_finished(
        &mut self,
        run: RunId,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let calls: Vec<_> = self
            .current(run)?
            .tools
            .open
            .iter()
            .filter_map(|open| {
                let p = open.provider.as_ref()?;
                let at = p.owner?;
                Some((open.call.clone(), at, p.after_records))
            })
            .collect();
        for (call, at, after_records) in calls {
            let association = self.current(run)?.draw_calls[at]
                .provider
                .as_mut()
                .expect("the stream owns the association");
            if association.finished || association.stopped {
                continue;
            }
            association.finished = true;
            let caller = self.current(run)?.model_caller(call.clone());
            if let Dispatch::Answered(_) = self.tool_input(
                caller,
                ToolName::Draw,
                CallInput::Draw(DrawAction::Finish(
                    after_records.then_some(CallError::Token(TokenError::AfterRecords)),
                )),
                effects,
            ) {
                return self.fail(run, crate::store::Failure::Engine, effects);
            }
            if !self.is_provider(run, &call) {
                return Ok(());
            }
        }
        Ok(())
    }

    pub(super) fn close_provider_round(&mut self, run: RunId, effects: &mut Vec<Effect>) {
        let calls: Vec<_> = self
            .current
            .as_ref()
            .filter(|current| current.id == run)
            .map(|current| {
                current
                    .tools
                    .open
                    .iter()
                    .filter(|open| open.provider.as_ref().is_some_and(|p| p.owner.is_some()))
                    .map(|open| open.call.clone())
                    .collect()
            })
            .unwrap_or_default();
        for call in calls {
            if let Some(current) = &mut self.current
                && let Some(p) = current
                    .tools
                    .open
                    .iter_mut()
                    .find(|open| open.call == call)
                    .and_then(|open| open.provider.as_mut())
            {
                p.registered = false;
            }
            if self
                .provider_invalid(run, &call, CallError::Interrupted, effects)
                .is_err()
            {
                effects.push(Effect::Broken("drawing cleanup lost its run owner"));
                return;
            }
            let owned = self.current.as_ref().and_then(|current| {
                current
                    .tools
                    .open
                    .iter()
                    .find(|open| open.call == call)
                    .and_then(|open| open.provider.as_ref())
                    .and_then(|p| p.owner)
                    .filter(|at| {
                        current.draw_calls[*at]
                            .provider
                            .as_ref()
                            .is_some_and(|owner| !owner.stopped)
                    })
                    .map(|at| (current.thread, current.draw_calls[at].call.clone()))
            });
            if let Some((thread, id)) = owned {
                match self.drawing.handle(Input::CallFinished {
                    thread,
                    run,
                    call: id,
                    error: Some(CallError::Interrupted),
                }) {
                    Ok(output) => self.drawing_output(output, effects),
                    Err(_) => effects.push(Effect::Broken("drawing cleanup lost its call owner")),
                }
            }
        }
    }
}

#[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
fn mark_close(run: RunId, index: u32, bytes: usize) {
    use crate::connections::model::text_observation::{DrawStage, Observation, REPLAY_STEPS};

    if let Some(observation) = Observation::drawing()
        && let Some(record) = usize::try_from(index)
            .ok()
            .and_then(|index| index.checked_sub(1))
        && record < REPLAY_STEPS.len()
    {
        observation.mark_draw(
            run,
            crate::platform::qpc_now(),
            DrawStage::Close { record, bytes },
        );
    }
}
