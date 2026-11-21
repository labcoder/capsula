use kumochi_protocol::message::{MAX_SEGMENTS_PER_RUN, SegmentStart};

use super::{
    DataToShell, HANDSHAKE_TIMEOUT, Instant, RunId, SessionIo, output::OutputOwner, thread,
};
use crate::speaker::queued::Packet;

#[derive(Default)]
pub(super) struct Reader {
    announced: Option<(RunId, u32)>,
}

impl Reader {
    pub(super) fn frame(
        &mut self,
        owner: &OutputOwner,
        session: u64,
        message: DataToShell,
        bytes: u32,
        credit: &SessionIo,
    ) -> Result<Option<DataToShell>, ()> {
        let (run, segment, output, mut packet) = match message {
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
                return owner
                    .end_queued_run(session, run)
                    .then_some(Some(message))
                    .ok_or(());
            }
            _ => return Ok(Some(message)),
        };
        let pcm = packet.is_some();
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
            if pcm {
                segment.0 >= *next
            } else {
                segment.0 != *next
            }
        }) {
            return Err(());
        }
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(());
            }
            if owner.queued_feed(session, run, output, &mut packet)? {
                if !pcm && let Some((_, next)) = cursor {
                    *next += 1;
                }
                if !pcm || packet.is_some() {
                    credit.grant(bytes);
                }
                return Ok(None);
            }
            thread::park_timeout(left);
        }
    }
}
