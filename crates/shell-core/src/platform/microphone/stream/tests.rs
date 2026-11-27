use std::sync::atomic::AtomicBool;
use std::time::Instant;

use async_channel::Sender;
use kumochi_protocol::message::{GestureId, MicPcm};

use super::super::pcm::{Format, Packetizer, Sample};
use super::super::{CollectionGate, Finalization, MicError, Mode, PacketBoundary, PacketStatus};
use super::{Stream, run};

const GESTURE: GestureId = GestureId(1);

struct Fake {
    stopped: Result<(), MicError>,
    checked: Result<(), MicError>,
    /// A gate the stream revokes as it is destroyed, as a native teardown racing a cancel would.
    cancels_on_drop: Option<CollectionGate>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        if let Some(gate) = &self.cancels_on_drop {
            gate.cancel(GESTURE);
        }
    }
}

impl Stream for Fake {
    fn format(&self) -> Format {
        Format {
            rate: 8_000,
            channels: 1,
            sample: Sample::I16,
        }
    }

    fn stop(&mut self) -> Result<(), MicError> {
        self.stopped
    }

    fn check(&self) -> Result<(), MicError> {
        self.checked
    }
}

const CLEAN: Fake = Fake {
    stopped: Ok(()),
    checked: Ok(()),
    cancels_on_drop: None,
};

struct Hold {
    result: Result<Finalization, MicError>,
    sent: Vec<(u32, bool, Vec<i16>)>,
    mode: Result<Mode, MicError>,
    high_water: usize,
}

/// One hold on an opened stream whose pump is `pump`, with the gate it may release.
fn hold(
    stream: Fake,
    pump: impl FnOnce(
        &CollectionGate,
        &mut Packetizer,
        &mut PacketBoundary,
        &Sender<MicPcm>,
    ) -> Result<(), MicError>,
) -> Hold {
    let gate = CollectionGate::default();
    let token = gate.arm(GESTURE).unwrap();
    let session = AtomicBool::new(true);
    let (output, queue) = async_channel::bounded(2);
    let mut boundary = PacketBoundary::default();
    let result = run(
        &token,
        &session,
        &output,
        &mut boundary,
        Ok(stream),
        |_, packets, boundary| pump(&gate, packets, boundary, &output),
    );
    let mut sent = Vec::new();
    while let Ok(packet) = queue.try_recv() {
        sent.push((packet.seq, packet.last, packet.samples));
    }
    Hold {
        result,
        sent,
        mode: token.mode(&session, Instant::now()),
        high_water: boundary.queue_high_water,
    }
}

/// Queues three samples, then releases the hold and ends the pump with `ended`.
fn speak_and_release(
    ended: Result<(), MicError>,
) -> impl FnOnce(
    &CollectionGate,
    &mut Packetizer,
    &mut PacketBoundary,
    &Sender<MicPcm>,
) -> Result<(), MicError> {
    move |gate, packets, _, output| {
        for sample in [17, -93, 405] {
            packets.push(sample, output)?;
        }
        gate.release(GESTURE);
        ended
    }
}

#[test]
fn a_released_hold_sends_its_audio_after_a_confirmed_stop_and_check() {
    let hold = hold(CLEAN, speak_and_release(Ok(())));
    assert_eq!(hold.result, Ok(Finalization::Released { packets: 1 }));
    assert_eq!(hold.sent, vec![(0, true, vec![17, -93, 405])]);
    assert_eq!(hold.mode, Ok(Mode::Release));
    assert_eq!(hold.high_water, 1);
}

#[test]
fn a_pump_revoked_after_release_still_releases_what_it_collected() {
    for revoked in [MicError::Cancelled, MicError::Stale] {
        let hold = hold(CLEAN, speak_and_release(Err(revoked)));
        assert_eq!(hold.result, Ok(Finalization::Released { packets: 1 }));
        assert_eq!(hold.sent, vec![(0, true, vec![17, -93, 405])]);
    }
}

#[test]
fn a_pump_failing_before_release_discards_and_cancels() {
    for error in [MicError::Cancelled, MicError::Unavailable] {
        let hold = hold(CLEAN, |_, packets, _, output| {
            packets.push(17, output)?;
            Err(error)
        });
        assert_eq!(hold.result, Ok(Finalization::Discarded(error)));
        assert_eq!(hold.sent, vec![]);
        assert_eq!(hold.mode, Err(MicError::Cancelled));
    }
}

#[test]
fn a_device_check_failing_after_stop_discards_the_collected_audio() {
    let changed = Fake {
        checked: Err(MicError::Format),
        cancels_on_drop: None,
        ..CLEAN
    };
    let hold = hold(changed, speak_and_release(Ok(())));
    assert_eq!(hold.result, Ok(Finalization::Discarded(MicError::Format)));
    assert_eq!(hold.sent, vec![]);
    assert_eq!(hold.mode, Err(MicError::Cancelled));
}

#[test]
fn a_failed_stop_outranks_a_failed_device_check() {
    let gone = Fake {
        stopped: Err(MicError::Unavailable),
        checked: Err(MicError::Format),
        cancels_on_drop: None,
    };
    let hold = hold(gone, speak_and_release(Ok(())));
    assert_eq!(
        hold.result,
        Ok(Finalization::Discarded(MicError::Unavailable))
    );
    assert_eq!(hold.sent, vec![]);
    assert_eq!(hold.mode, Err(MicError::Cancelled));
}

#[test]
fn an_unconfirmed_stop_or_pump_is_an_error_and_never_a_receipt() {
    for (pumped, stopped) in [
        (Ok(()), Err(MicError::Unconfirmed)),
        (Err(MicError::Unavailable), Err(MicError::Unconfirmed)),
        (Err(MicError::Unconfirmed), Ok(())),
    ] {
        let unknown = Fake {
            stopped,
            cancels_on_drop: None,
            ..CLEAN
        };
        let hold = hold(unknown, speak_and_release(pumped));
        assert_eq!(hold.result, Err(MicError::Unconfirmed));
        assert_eq!(hold.sent, vec![]);
    }
}

#[test]
fn a_cancel_while_the_stream_is_destroyed_discards_the_last_packet() {
    let gate = CollectionGate::default();
    let token = gate.arm(GESTURE).unwrap();
    let session = AtomicBool::new(true);
    let (output, queue) = async_channel::bounded(2);
    let stream = Fake {
        cancels_on_drop: Some(gate.clone()),
        ..CLEAN
    };
    let result = run(
        &token,
        &session,
        &output,
        &mut PacketBoundary::default(),
        Ok(stream),
        |_, packets, boundary| speak_and_release(Ok(()))(&gate, packets, boundary, &output),
    );
    assert_eq!(result, Ok(Finalization::Discarded(MicError::Cancelled)));
    assert!(queue.is_empty());
}

#[test]
fn a_fault_the_boundary_latched_discards_a_pump_that_ended_cleanly() {
    let hold = hold(CLEAN, |gate, packets, boundary, output| {
        packets.push(17, output)?;
        let faulted = PacketStatus {
            frames: 1,
            position: 0,
            captured_at: 0,
            discontinuity: false,
            timestamp_error: true,
        };
        assert_eq!(boundary.packet(None, faulted), Err(MicError::Timestamp));
        gate.release(GESTURE);
        Ok(())
    });
    assert_eq!(
        hold.result,
        Ok(Finalization::Discarded(MicError::Timestamp))
    );
    assert_eq!(hold.sent, vec![]);
    assert_eq!(hold.mode, Err(MicError::Cancelled));
}

#[test]
fn a_pump_that_ends_without_release_discards_its_audio() {
    let hold = hold(CLEAN, |_, packets, _, output| packets.push(17, output));
    assert_eq!(
        hold.result,
        Ok(Finalization::Discarded(MicError::Cancelled))
    );
    assert_eq!(hold.sent, vec![]);
}

#[test]
fn a_full_queue_at_release_discards_and_cancels() {
    let hold = hold(CLEAN, |gate, packets, _, output| {
        for seq in 0..2 {
            output
                .try_send(MicPcm {
                    gesture: GESTURE,
                    seq,
                    rate: 8_000,
                    last: false,
                    samples: vec![1],
                })
                .unwrap();
        }
        packets.push(17, output)?;
        gate.release(GESTURE);
        Ok(())
    });
    assert_eq!(hold.result, Ok(Finalization::Discarded(MicError::Overflow)));
    assert_eq!(hold.sent, vec![(0, false, vec![1]), (1, false, vec![1])]);
    assert_eq!(hold.mode, Err(MicError::Cancelled));
}

#[test]
fn a_stream_that_never_opened_releases_empty_only_when_revoked_by_release() {
    for (error, released, finalization) in [
        (
            MicError::Cancelled,
            true,
            Finalization::Released { packets: 0 },
        ),
        (MicError::Stale, true, Finalization::Released { packets: 0 }),
        (
            MicError::Unavailable,
            true,
            Finalization::Discarded(MicError::Unavailable),
        ),
        (
            MicError::Cancelled,
            false,
            Finalization::Discarded(MicError::Cancelled),
        ),
    ] {
        let gate = CollectionGate::default();
        let token = gate.arm(GESTURE).unwrap();
        if released {
            gate.release(GESTURE);
        }
        let (output, queue) = async_channel::bounded(2);
        let result = run::<Fake>(
            &token,
            &AtomicBool::new(true),
            &output,
            &mut PacketBoundary::default(),
            Err(error),
            |_, _, _| panic!("a stream that never opened has nothing to collect"),
        );
        assert_eq!(result, Ok(finalization));
        assert!(queue.is_empty());
    }
}
