use std::num::NonZeroU64;

use crate::presenter::RetirementLane;

use kumochi_protocol::message::{
    MAX_SEGMENTS_PER_RUN, SegmentId, SegmentStart, SpeechPromise, StepRef,
};

use super::{
    DataToShell, HANDSHAKE_TIMEOUT, Instant, RunId, SessionIo,
    narrated::{Narrated, valid_key},
    output::{NarratedReservation, OutputOwner},
    thread,
};
use crate::presenter::DrawKey;
use crate::speaker::conductor::Assignment;
use crate::speaker::queued::Packet;

#[derive(Clone, Copy)]
pub(super) enum MetadataFrame {
    Commit,
    Announcement,
}

#[derive(Default)]
pub(super) struct Reader {
    announced: Option<(RunId, u32)>,
    retirements: [u64; 2],
}

impl Reader {
    pub(super) fn frame(
        &mut self,
        owner: &OutputOwner,
        session: u64,
        message: DataToShell,
        bytes: u32,
        credit: &SessionIo,
        narrated: Option<&Narrated>,
    ) -> Result<Option<DataToShell>, ()> {
        let mut assignment = None;
        let (run, segment, output, mut packet) = match message {
            DataToShell::SceneStepCommit {
                run,
                scene,
                index,
                speech,
                ..
            } if narrated.is_some() => {
                Self::commit(
                    owner,
                    narrated.ok_or(())?,
                    session,
                    DrawKey { run, scene, index },
                    speech,
                )?;
                return Ok(Some(message));
            }
            DataToShell::SegmentQueued {
                start: SegmentStart::Speech,
                run,
                segment,
                output_generation,
                step: Some(step),
            } if narrated.is_some() => {
                let expected = Assignment {
                    session,
                    output: output_generation,
                    run,
                    segment,
                    step,
                };
                assignment = Some(expected);
                (run, segment, output_generation, None)
            }
            DataToShell::SegmentQueued {
                start: SegmentStart::Plain,
                run,
                segment,
                output_generation,
                ..
            } => (run, segment, output_generation, None),
            DataToShell::SpeechPcm(pcm) => (
                pcm.run,
                pcm.segment,
                pcm.output_generation,
                Some(Packet::wire(pcm, session, bytes)),
            ),
            DataToShell::SegmentQueued { .. }
            | DataToShell::SceneStepCommit {
                speech: Some(_), ..
            } => return Err(()),
            DataToShell::RunEnd { run } => {
                if let Some(bridge) = narrated {
                    bridge.run_end(run);
                }
                return owner
                    .end_queued_run(session, run)
                    .then_some(Some(message))
                    .ok_or(());
            }
            _ => {
                if let Some(bridge) = narrated {
                    self.retirement(bridge, &message)?;
                }
                return Ok(Some(message));
            }
        };
        let pcm = packet.is_some();
        let cursor = self.accept_segment(run, segment, packet.as_ref())?;
        wait_for(|| {
            if let Some(assignment) = assignment {
                Self::metadata(
                    owner,
                    narrated.ok_or(())?,
                    assignment,
                    MetadataFrame::Announcement,
                )
            } else {
                owner.queued_feed(session, run, output, segment, &mut packet)
            }
        })?;
        if !pcm && let Some((_, next)) = cursor {
            *next += 1;
        }
        if !pcm || packet.is_some() {
            credit.grant(bytes);
        }
        Ok(None)
    }

    fn retirement(&mut self, narrated: &Narrated, message: &DataToShell) -> Result<(), ()> {
        let (scenes, successor, lane) = match message {
            DataToShell::ScenesRemoved { scenes } => {
                (scenes.as_slice(), None, RetirementLane::Scenes)
            }
            DataToShell::ScenesReplaced { scenes, successor } => {
                (scenes.as_slice(), Some(*successor), RetirementLane::Scenes)
            }
            DataToShell::SceneCutApplied { scene, cut, .. } if cut.activated == 0 => {
                (std::slice::from_ref(scene), None, RetirementLane::Cut)
            }
            _ => return Ok(()),
        };
        let pending = &mut self.retirements[lane.slot()];
        let ordinal = pending.checked_add(1).and_then(NonZeroU64::new).ok_or(())?;
        if !narrated.retiring(scenes, successor, lane, ordinal) {
            return Err(());
        }
        *pending = ordinal.get();
        Ok(())
    }

    fn metadata(
        owner: &OutputOwner,
        narrated: &Narrated,
        assignment: Assignment,
        frame: MetadataFrame,
    ) -> Result<bool, ()> {
        if matches!(frame, MetadataFrame::Commit) && !narrated.can_promise(assignment.run)? {
            return Ok(false);
        }
        match owner.narrated_metadata(assignment, frame)? {
            NarratedReservation::Waiting => Ok(false),
            NarratedReservation::Reserved => match frame {
                MetadataFrame::Commit => narrated.promised(assignment),
                MetadataFrame::Announcement => narrated.announce(assignment),
            }
            .then_some(true)
            .ok_or(()),
            NarratedReservation::Drained => Ok(true),
        }
    }

    fn commit(
        owner: &OutputOwner,
        narrated: &Narrated,
        session: u64,
        key: DrawKey,
        speech: Option<SpeechPromise>,
    ) -> Result<(), ()> {
        let Some(promise) = speech else {
            return wait_for(|| narrated.unassigned(key));
        };
        let assignment = Assignment {
            session,
            output: promise.output_generation,
            run: key.run,
            segment: promise.segment,
            step: StepRef {
                scene: key.scene,
                index: key.index,
            },
        };
        if assignment.segment.0 >= MAX_SEGMENTS_PER_RUN || !valid_key(key) {
            return Err(());
        }
        wait_for(|| Self::metadata(owner, narrated, assignment, MetadataFrame::Commit))
    }

    fn accept_segment(
        &mut self,
        run: RunId,
        segment: SegmentId,
        packet: Option<&Packet>,
    ) -> Result<Option<&mut (RunId, u32)>, ()> {
        if segment.0 >= MAX_SEGMENTS_PER_RUN {
            return Err(());
        }
        if self.announced.is_none_or(|(current, _)| run > current) {
            self.announced = Some((run, 0));
        }
        let cursor = self
            .announced
            .as_mut()
            .filter(|(current, _)| run == *current);
        if cursor.as_ref().is_some_and(|(_, next)| {
            if packet.is_some() {
                segment.0 >= *next
            } else {
                segment.0 != *next
            }
        }) {
            return Err(());
        }
        Ok(cursor)
    }
}

fn wait_for(mut ready: impl FnMut() -> Result<bool, ()>) -> Result<(), ()> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(());
        }
        if ready()? {
            return Ok(());
        }
        thread::park_timeout(left);
    }
}
