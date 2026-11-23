use super::{
    AtomicU8, BLOCKS, ErrorKind, MAX_FRAMES, MAX_RATE, OutputError, OutputEvent, PACKET_SAMPLES,
    PRESENTATION_SLOTS, PacketKind, ReadyBlock, SOURCE_RATE,
};

fn pair(rate: u32, channels: usize) -> (super::Submit, super::Playback) {
    super::pair(rate, channels, std::sync::Arc::default())
}

impl super::Playback {
    fn fill(&mut self, out: &mut [f32]) {
        self.fill_at(out, None);
    }
}

#[test]
fn render_copies_prepared_stereo_frames_across_callback_boundaries_and_fills_silence() {
    let (mut submit, mut render) = pair(SOURCE_RATE, 2);
    submit.packet(&[i16::MIN, 16_384], PacketKind::End).unwrap();

    let mut first = [99.0; 2];
    render.fill(&mut first);
    assert_eq!(first.map(f32::to_bits), [-1.0, -1.0].map(f32::to_bits));
    assert_eq!(submit.free.len(), BLOCKS - 1);

    let mut second = [99.0; 4];
    render.fill(&mut second);
    assert_eq!(
        second.map(f32::to_bits),
        [0.5, 0.5, 0.0, 0.0].map(f32::to_bits)
    );
    assert_eq!(submit.free.len(), BLOCKS);

    let (mut submit, mut render) = pair(SOURCE_RATE, 8);
    submit.packet(&[16_384], PacketKind::End).unwrap();
    let mut surround = [99.0; 8];
    render.fill(&mut surround);
    assert_eq!(
        surround.map(f32::to_bits),
        [0.5, 0.5, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0].map(f32::to_bits)
    );
}

#[test]
fn differing_endpoint_rate_and_segment_end_reach_the_callback() {
    let (mut submit, mut render) = pair(48_000, 1);
    submit.packet(&[16_384; 120], PacketKind::More).unwrap();
    submit.packet(&[], PacketKind::End).unwrap();

    let mut out = [99.0; 242];
    render.fill(&mut out);
    assert!(
        out[50..190]
            .iter()
            .all(|sample| (*sample - 0.5).abs() < 0.002),
        "resampling should preserve the interior of a constant signal"
    );
    assert!(out[239] > 0.2, "the last in-range sample must be emitted");
    assert_eq!(out[240..], [0.0; 2]);

    submit.packet(&[0; 120], PacketKind::End).unwrap();
    render.fill(&mut out);
    assert_eq!(
        out.map(f32::to_bits),
        [0; 242],
        "the next segment must not reuse old history"
    );
}

#[test]
fn backpressure_preserves_refused_packets_and_the_size_limit_is_exact() {
    let (mut submit, mut render) = pair(SOURCE_RATE, 1);
    assert_eq!(
        submit.packet(&[0; PACKET_SAMPLES + 1], PacketKind::More),
        Err(OutputError::PacketTooLarge)
    );
    for _ in 0..BLOCKS {
        submit
            .packet(&[16_384; PACKET_SAMPLES], PacketKind::More)
            .unwrap();
    }
    assert_eq!(
        submit.packet(&[i16::MIN], PacketKind::End),
        Err(OutputError::Full)
    );

    let mut one = [0.0; PACKET_SAMPLES];
    render.fill(&mut one);
    assert_eq!(one.map(f32::to_bits), [0.5f32.to_bits(); PACKET_SAMPLES]);
    submit.packet(&[i16::MIN], PacketKind::End).unwrap();
    let mut rest = [99.0; (BLOCKS - 1) * PACKET_SAMPLES + 2];
    render.fill(&mut rest);
    let old = (BLOCKS - 1) * PACKET_SAMPLES;
    assert!(
        rest[..old]
            .iter()
            .all(|&sample| sample.to_bits() == 0.5f32.to_bits())
    );
    assert_eq!(rest[old..], [-1.0, 0.0]);
}

#[test]
fn maximum_rate_full_final_packet_does_not_grow_the_prepared_buffers() {
    let (mut submit, mut render) = pair(MAX_RATE, 2);
    submit
        .packet(&[16_384; PACKET_SAMPLES], PacketKind::More)
        .unwrap();
    let (final_submission, requests) = crate::allocation::requests(64, || {
        submit.packet(&[16_384; PACKET_SAMPLES], PacketKind::End)
    });
    assert_eq!(final_submission, Ok(()));
    assert!(
        requests.iter().all(|request| request.action != 0),
        "a prepared submission must not grow its buffers"
    );

    let mut rendered = vec![99.0; 7_680 * 2 + 2];
    render.fill(&mut rendered);
    let (audio, silence) = rendered.split_at(7_680 * 2);
    for frame in audio.as_chunks::<2>().0 {
        assert!(frame[0].is_finite() && frame[0] > 0.0);
        assert_eq!(frame[0].to_bits(), frame[1].to_bits());
    }
    assert_eq!(silence, [0.0; 2]);
}

#[test]
fn a_late_xrun_cannot_hide_stream_loss() {
    let pending = AtomicU8::new(0);
    OutputEvent::latch(&pending, ErrorKind::DeviceChanged);
    assert_eq!(OutputEvent::take(&pending), Some(OutputEvent::Rerouted));
    OutputEvent::latch(&pending, ErrorKind::StreamInvalidated);
    OutputEvent::latch(&pending, ErrorKind::Xrun);
    assert_eq!(OutputEvent::take(&pending), Some(OutputEvent::Lost));
    assert_eq!(OutputEvent::take(&pending), None);
}

#[test]
fn an_unpublished_block_is_silent_until_the_next_callback() {
    let (mut submit, mut playback) = pair(SOURCE_RATE, 2);
    let index = submit.free.pop().unwrap();
    std::sync::Arc::get_mut(&mut submit.blocks[index])
        .unwrap()
        .samples
        .extend([0.5; 4]);
    let ready = ReadyBlock {
        index,
        pcm: std::sync::Arc::clone(&submit.blocks[index]),
        stamp: None,
    };

    let mut pending = [99.0; 4];
    let result = submit.ready.push_before_publish(ready, || {
        assert!(playback.ready.pop().is_none(), "unpublished is empty");
        playback.fill(&mut pending);
    });
    assert!(result.is_ok(), "the reserved slot must publish");
    assert_eq!(pending.map(f32::to_bits), [0; 4]);

    let mut published = [99.0; 4];
    playback.fill(&mut published);
    assert_eq!(published.map(f32::to_bits), [0.5f32.to_bits(); 4]);
}

#[test]
fn dropping_partial_playback_leaves_pcm_for_worker_teardown() {
    let (mut submit, mut playback) = pair(SOURCE_RATE, 2);
    submit.packet(&[16_384, i16::MIN], PacketKind::End).unwrap();
    let mut one_frame = [99.0; 2];
    playback.fill(&mut one_frame);
    assert_eq!(one_frame.map(f32::to_bits), [0.5f32.to_bits(); 2]);

    let index = playback.current.as_ref().unwrap().index;
    let ((), callback_activity) = crate::allocation::activity(|| drop(playback));
    assert_eq!(
        callback_activity,
        [0, 0],
        "callback teardown must not free PCM"
    );
    assert_eq!(std::sync::Arc::strong_count(&submit.blocks[index]), 1);
    assert_eq!(
        submit.blocks[index]
            .samples
            .iter()
            .map(|sample| sample.to_bits())
            .collect::<Vec<_>>(),
        [0.5f32, 0.5, -1.0, -1.0].map(f32::to_bits),
        "the callback must leave the worker's zeroizing block intact"
    );

    let target = submit.blocks[index].samples.as_ptr().addr();
    let bytes = MAX_FRAMES * 2 * size_of::<f32>();
    let ((), releases) = crate::allocation::requests(64, || drop(submit));
    let pcm_releases = releases
        .iter()
        .filter(|event| event.action == 1 && event.size == bytes);
    assert_eq!(pcm_releases.clone().count(), BLOCKS);
    assert_eq!(
        pcm_releases.filter(|event| event.pointer == target).count(),
        1
    );
}

#[test]
fn a_callback_retained_past_wrapper_drop_cannot_play() {
    for path in [DropPath::Normal, DropPath::Unwind] {
        retained_wrapper(path);
    }
}

#[derive(Clone, Copy)]
enum DropPath {
    Normal,
    Unwind,
}

struct ClearProbe;

impl Drop for ClearProbe {
    fn drop(&mut self) {
        super::CLEAR_OBSERVER.set(None);
    }
}

fn retained_wrapper(path: DropPath) {
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    use super::lifetime::{OwnedStream, Owner, Permit};
    use crate::allocation;

    static NORMAL: AtomicBool = AtomicBool::new(false);
    static UNWIND: AtomicBool = AtomicBool::new(false);
    let busy = match path {
        DropPath::Normal => &NORMAL,
        DropPath::Unwind => &UNWIND,
    };
    let (mut submit, mut playback) = pair(SOURCE_RATE, 2);
    for index in 0..BLOCKS {
        let samples = if index == 1 {
            &[16_384, i16::MIN][..]
        } else {
            &[8192][..]
        };
        submit.packet(samples, PacketKind::End).unwrap();
    }
    let blocks: [usize; BLOCKS] =
        std::array::from_fn(|index| submit.blocks[index].samples.as_ptr().addr());
    let mut head = [99.0; 4];
    playback.fill(&mut head);
    assert_eq!(
        head.map(f32::to_bits),
        [0.25, 0.25, 0.5, 0.5].map(f32::to_bits)
    );

    super::CLEAR_OBSERVER.set(Some(|pointer| allocation::handoff(pointer, 0, 0)));
    let _clear_probe = ClearProbe;
    let (sender, receiver) = mpsc::sync_channel(1);
    let notices = std::sync::Arc::clone(&submit.shared);
    let wrapper = OwnedStream {
        stream: (),
        owner: Owner::new(submit, Permit::claim(busy).unwrap(), sender),
    };
    let (unwound, closing) = allocation::requests(128, || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _wrapper = wrapper;
            assert!(matches!(path, DropPath::Normal), "unwind the wrapper");
        }))
    });
    assert_eq!(unwound.is_err(), matches!(path, DropPath::Unwind));
    let mut parked = receiver.try_recv().unwrap();
    assert!(!parked.acknowledged());
    assert!(Permit::claim(busy).is_none());
    assert!(parked.submit.blocks[0].samples.is_empty());
    assert_eq!(parked.submit.blocks[1].samples.len(), 4);

    let mut late = [99.0; 6];
    let ((), callback_activity) = allocation::activity(|| playback.fill(&mut late));
    assert_eq!(
        late.map(f32::to_bits),
        [0; 6],
        "retained callback must be silent"
    );
    assert_eq!(callback_activity, [0, 0]);
    assert_eq!(playback.current.as_ref().unwrap().index, 1);
    assert_eq!(playback.offset, 2);
    assert_eq!(playback.ready.len(), BLOCKS - 2);

    let ((), callback_events) = allocation::requests(64, || drop(playback));
    assert!(
        callback_events.is_empty(),
        "the callback must neither clear nor free a block"
    );
    assert!(!parked.acknowledged());
    let ((), late_scrub) = allocation::requests(64, || assert!(!parked.poll_retirement()));
    assert!(parked.submit.blocks[1].samples.is_empty());
    assert!(Permit::claim(busy).is_none());
    drop(notices);
    assert!(parked.acknowledged());

    let ((), freed) = allocation::requests(64, || drop(parked));
    for pointer in blocks {
        assert_eq!(
            closing
                .iter()
                .chain(&late_scrub)
                .chain(&freed)
                .filter(|e| e.action == 4 && e.pointer == pointer)
                .count(),
            1
        );
        assert_eq!(
            closing
                .iter()
                .filter(|e| e.action == 1 && e.pointer == pointer)
                .count(),
            0
        );
        assert_eq!(
            freed
                .iter()
                .filter(|e| e.action == 1 && e.pointer == pointer)
                .count(),
            1
        );
    }
    assert!(Permit::claim(busy).is_some());
}

#[test]
fn stop_discards_partial_and_ready_blocks_then_reports_the_last_ramp_frame() {
    use std::time::Duration;

    use super::TailAck;

    let (mut submit, mut playback) = pair(SOURCE_RATE, 2);
    submit.packet(&[16_384; 240], PacketKind::End).unwrap();
    submit.packet(&[i16::MIN; 60], PacketKind::End).unwrap();
    let mut first = [0.0; 2];
    playback.fill_at(&mut first, Some(Duration::from_millis(10)));
    assert_eq!(first.map(f32::to_bits), [0.5f32.to_bits(); 2]);

    submit.control.stop();
    assert_eq!(submit.control.tail(), TailAck::Pending);
    let start = Duration::from_millis(20);
    let mut out = [99.0; 300];
    playback.fill_at(&mut out, Some(start));
    assert!(
        out[..238]
            .iter()
            .all(|sample| *sample > 0.0 && *sample < 0.5)
    );
    assert!(out[238..].iter().all(|sample| sample.to_bits() == 0));
    assert_eq!(
        submit.control.tail(),
        TailAck::At(start + Duration::from_nanos(118 * 1_000_000_000 / u64::from(SOURCE_RATE)))
    );
    assert_eq!(submit.free.len(), BLOCKS);
}

#[test]
fn a_stop_between_bounded_copies_fences_the_remainder_of_the_same_block() {
    use std::time::Duration;

    let (mut submit, mut playback) = pair(SOURCE_RATE, 1);
    submit.packet(&[16_384; 480], PacketKind::End).unwrap();
    let ctrl = std::sync::Arc::clone(&submit.control);
    let mut out = [99.0; 210];
    let mut copies = 0;
    playback.render(
        &mut out,
        Some(Duration::from_millis(11)),
        super::Control::begin_copy,
        || {
            copies += 1;
            if copies == 1 {
                ctrl.stop();
            }
        },
    );

    assert_eq!(copies, 1);
    assert_eq!(
        out[..60]
            .iter()
            .filter(|s| s.to_bits() == 0.5f32.to_bits())
            .count(),
        60
    );
    assert!(out[60..179].iter().all(|s| *s > 0.0 && *s < 0.5));
    assert!(out[179..].iter().all(|s| s.to_bits() == 0));
    assert_eq!(submit.free.len(), BLOCKS);
}

#[test]
fn stop_without_callback_timing_latches_unknown_not_quiet() {
    use super::TailAck;

    let (mut submit, mut playback) = pair(SOURCE_RATE, 1);
    submit.packet(&[16_384; 120], PacketKind::End).unwrap();
    playback.fill_at(&mut [0.0; 1], None);
    submit.control.stop();
    playback.fill_at(&mut [0.0; 120], None);
    assert_eq!(submit.control.tail(), TailAck::Unknown);
}

#[test]
fn callback_source_spans_follow_rendered_frames_not_converter_intake() {
    use std::time::Duration;

    use kumochi_protocol::message::{RunId, SegmentId};

    use super::{Presented, Source};

    let (mut submit, mut playback) = pair(48_000, 1);
    let source = Source {
        run: RunId(5),
        generation: 3,
        segment: SegmentId(8),
        at: 100,
    };
    let start = Duration::from_millis(10);
    submit
        .packet_for(&[16_384; 120], PacketKind::More, Some(source))
        .unwrap();
    playback.fill_at(&mut [0.0], Some(start));
    assert_eq!(submit.control.handed(), 101);
    assert_eq!(submit.observed.pop(), None);
    playback.fill_at(&mut [0.0; 31], Some(start));
    assert_eq!(submit.control.handed(), 116);
    assert_eq!(
        submit.observed.pop(),
        Some(Presented {
            source,
            first: 100,
            through: 116,
            at: Some(start + Duration::from_micros(625))
        })
    );

    let end = Duration::from_millis(11);
    submit
        .packet_for(&[], PacketKind::End, Some(Source { at: 220, ..source }))
        .unwrap();
    playback.fill_at(&mut [0.0; 210], Some(end));
    assert_eq!(submit.control.handed(), 220);
    let final_span = submit.observed.pop().unwrap();
    assert_eq!((final_span.first, final_span.through), (116, 220));
    assert_eq!(
        final_span.at,
        Some(end + Duration::from_nanos(207 * 1_000_000_000 / 48_000))
    );
    assert_eq!(submit.observed.pop(), None);
}

#[test]
fn missing_device_time_reports_handoff_without_confirming_presentation() {
    use kumochi_protocol::message::{RunId, SegmentId};

    use super::{Presented, Source};

    let (mut submit, mut playback) = pair(SOURCE_RATE, 1);
    let source = Source {
        run: RunId(5),
        generation: 3,
        segment: SegmentId(8),
        at: 100,
    };
    submit
        .packet_for(&[16_384; 2], PacketKind::End, Some(source))
        .unwrap();
    playback.fill_at(&mut [0.0; 5], None);
    assert_eq!(submit.control.handed(), 102);
    assert_eq!(
        submit.observed.pop(),
        Some(Presented {
            source,
            first: 100,
            through: 102,
            at: None,
        })
    );
}

#[test]
fn recovery_fence_waits_for_an_in_flight_copy_without_admitting_another() {
    let ctrl = super::Control::default();
    assert_eq!(ctrl.begin_copy(), Ok(()));
    ctrl.stop();
    assert_eq!(ctrl.fenced_handoff(), None);
    ctrl.handed_through(41);
    ctrl.end_copy();
    assert_eq!(ctrl.fenced_handoff(), Some(41));
    assert_eq!(ctrl.begin_copy(), Err(super::CopyDenied::Stop));
}

#[test]
fn inhibit_writes_silence_without_consuming_the_retained_block() {
    let (mut submit, mut playback) = pair(SOURCE_RATE, 1);
    submit
        .packet(&[8_192, 16_384, 24_576], PacketKind::End)
        .unwrap();
    let mut out = [99.0; 1];
    playback.fill(&mut out);
    assert_eq!(out.map(f32::to_bits), [0.25f32.to_bits()]);
    submit.control.inhibit();
    let mut out = [99.0; 3];
    playback.fill(&mut out);
    assert_eq!(out.map(f32::to_bits), [0; 3]);
    submit.control.resume();
    playback.fill(&mut out);
    assert_eq!(
        out.map(f32::to_bits),
        [0.5f32.to_bits(), 0.75f32.to_bits(), 0]
    );
}

#[test]
fn a_rejected_copy_admission_silences_this_callback_without_retrying() {
    let (mut submit, mut playback) = pair(SOURCE_RATE, 1);
    submit.packet(&[8_192, 16_384], PacketKind::End).unwrap();
    let mut attempts = 0;
    let mut out = [99.0];
    playback.render(
        &mut out,
        None,
        |control| {
            attempts += 1;
            if attempts == 1 {
                control.inhibit();
                let admitted = control.begin_copy();
                control.resume();
                admitted
            } else {
                control.begin_copy()
            }
        },
        || {},
    );
    assert_eq!(attempts, 1);
    assert_eq!(out[0].to_bits(), 0);
    playback.fill(&mut out);
    assert_eq!(out[0].to_bits(), 0.25f32.to_bits());
}

#[test]
fn dropping_the_backend_before_the_transport_silences_a_surviving_callback() {
    use super::lifetime::{Owner, Permit, cleanup_sender};
    use super::{BackendStream, Source, Transport};
    use kumochi_protocol::message::{RunId, SegmentId};
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    static OCCUPIED: AtomicBool = AtomicBool::new(false);
    let (mut submit, mut playback) = pair(SOURCE_RATE, 1);
    let source = Source {
        run: RunId(5),
        generation: 3,
        segment: SegmentId(0),
        at: 100,
    };
    submit
        .packet_for(&[8_192, 16_384], PacketKind::End, Some(source))
        .unwrap();
    submit
        .packet_for(
            &[24_576],
            PacketKind::End,
            Some(Source {
                at: 102,
                segment: SegmentId(1),
                ..source
            }),
        )
        .unwrap();
    let transport = Transport(Owner::new(
        submit,
        Permit::claim(&OCCUPIED).unwrap(),
        cleanup_sender().unwrap(),
    ));
    playback.fill_at(&mut [0.0; 1], Some(Duration::ZERO));
    assert_eq!(transport.control().handed(), 101);
    let backend = BackendStream((), std::sync::Arc::clone(&playback.shared.cancelled));
    drop(backend);

    let mut out = [99.0; 2];
    playback.fill_at(&mut out, Some(Duration::from_millis(1)));
    assert_eq!(out.map(f32::to_bits), [0; 2]);
    assert_eq!(transport.control().handed(), 101);
}

#[test]
fn saturated_presentation_keeps_exact_pcm_across_a_recycled_segment() {
    use super::Source;
    use kumochi_protocol::message::{RunId, SegmentId};
    use std::time::Duration;

    let (mut submit, mut playback) = pair(SOURCE_RATE, 1);
    let full = u64::try_from(PRESENTATION_SLOTS).unwrap();
    let first = Source {
        run: RunId(5),
        generation: 3,
        segment: SegmentId(0),
        at: 0,
    };
    let second = Source {
        segment: SegmentId(1),
        at: full + 1,
        ..first
    };
    submit
        .packet_for(
            &[16_384; PRESENTATION_SLOTS + 1],
            PacketKind::End,
            Some(first),
        )
        .unwrap();
    submit
        .packet_for(&[8192; 2], PacketKind::End, Some(second))
        .unwrap();
    for _ in 0..PRESENTATION_SLOTS {
        let mut out = [0.0];
        playback.fill_at(&mut out, Some(Duration::ZERO));
        assert_eq!(out.map(f32::to_bits), [0.5f32.to_bits()]);
    }
    let recycled = playback.current.as_ref().unwrap().index;
    let mut blocked = [99.0; 3];
    playback.fill_at(&mut blocked, Some(Duration::from_millis(1)));
    assert_eq!(blocked.map(f32::to_bits), [0; 3]);
    assert_eq!(submit.control.handed(), full);
    assert!(!submit.control.overflowed());
    for _ in 2..BLOCKS {
        submit.packet(&[0], PacketKind::End).unwrap();
    }

    let first_receipt = submit.observed.pop().unwrap();
    assert_eq!((first_receipt.first, first_receipt.through), (0, 1));
    let mut boundary = [99.0; 3];
    playback.fill_at(&mut boundary, Some(Duration::from_millis(2)));
    assert_eq!(boundary.map(f32::to_bits), [0.5f32.to_bits(), 0, 0]);
    assert_eq!(submit.control.handed(), second.at);
    let third = Source {
        segment: SegmentId(2),
        at: second.at + 2,
        ..first
    };
    submit
        .packet_for(&[-16_384], PacketKind::End, Some(third))
        .unwrap();
    assert_eq!(
        submit.blocks[recycled].samples[0].to_bits(),
        (-0.5f32).to_bits()
    );
    for through in 2..=second.at {
        let receipt = submit.observed.pop().unwrap();
        assert_eq!(receipt.source, first);
        assert_eq!((receipt.first, receipt.through), (through - 1, through));
    }
    let mut resumed = [99.0; BLOCKS + 2];
    playback.fill_at(&mut resumed, Some(Duration::from_millis(3)));
    let mut expected = [0.0; BLOCKS + 2];
    expected[..2].fill(0.25);
    expected[BLOCKS] = -0.5;
    assert_eq!(resumed.map(f32::to_bits), expected.map(f32::to_bits));
    let tail: Vec<_> = std::iter::from_fn(|| submit.observed.pop())
        .map(|receipt| (receipt.source, receipt.first, receipt.through))
        .collect();
    assert_eq!(
        tail,
        [
            (second, second.at, third.at),
            (third, third.at, third.at + 1)
        ]
    );
    assert!(!submit.control.overflowed());
}

#[test]
fn a_full_presentation_ring_does_not_delay_stop_fade_or_publish_extra_source() {
    use super::{Source, TailAck};
    use kumochi_protocol::message::{RunId, SegmentId};
    use std::time::Duration;

    let (mut submit, mut playback) = pair(SOURCE_RATE, 1);
    let source = Source {
        run: RunId(5),
        generation: 3,
        segment: SegmentId(0),
        at: 0,
    };
    submit
        .packet_for(
            &[16_384; PRESENTATION_SLOTS + 1],
            PacketKind::End,
            Some(source),
        )
        .unwrap();
    for _ in 0..PRESENTATION_SLOTS {
        playback.fill_at(&mut [0.0], Some(Duration::ZERO));
    }
    let full = u64::try_from(PRESENTATION_SLOTS).unwrap();
    assert_eq!(submit.control.handed(), full);
    submit.control.stop();
    let start = Duration::from_millis(20);
    let mut fade = [99.0; 120];
    playback.fill_at(&mut fade, Some(start));
    assert!(fade[..119].iter().all(|frame| *frame > 0.0 && *frame < 0.5));
    assert_eq!(fade[119].to_bits(), 0);
    assert_eq!(
        submit.control.tail(),
        TailAck::At(start + Duration::from_nanos(118 * 1_000_000_000 / u64::from(SOURCE_RATE)))
    );
    assert_eq!(submit.control.fenced_handoff(), Some(full));
    for through in 1..=full {
        let receipt = submit.observed.pop().unwrap();
        assert_eq!(
            (receipt.source, receipt.first, receipt.through),
            (source, through - 1, through)
        );
    }
    assert_eq!(submit.observed.pop(), None);
    assert!(!submit.control.overflowed());
    let mut after = [99.0; 4];
    playback.fill_at(&mut after, Some(start));
    assert_eq!(after.map(f32::to_bits), [0; 4]);
    assert_eq!(submit.control.fenced_handoff(), Some(full));
}

#[test]
fn receipt_pressure_at_an_unequal_rate_keeps_the_fractional_resume_position() {
    use super::Source;
    use kumochi_protocol::message::{RunId, SegmentId};
    use std::time::Duration;

    let (mut submit, mut playback) = pair(48_000, 1);
    let full = u64::try_from(PRESENTATION_SLOTS).unwrap();
    let source = Source {
        run: RunId(5),
        generation: 3,
        segment: SegmentId(0),
        at: 0,
    };
    submit
        .packet_for(&[16_384; 120], PacketKind::End, Some(source))
        .unwrap();
    let time = Some(Duration::ZERO);
    for _ in 0..PRESENTATION_SLOTS {
        playback.fill_at(&mut [0.0; 2], time);
    }
    let mut blocked = [99.0];
    playback.fill_at(&mut blocked, time);
    assert_eq!(blocked.map(f32::to_bits), [0]);
    assert_eq!(submit.control.handed(), full);
    for through in 1..=full {
        assert_eq!(submit.observed.pop().unwrap().through, through);
    }
    playback.fill_at(&mut [0.0], time);
    assert_eq!(submit.control.handed(), full + 1);
    assert_eq!(submit.observed.pop(), None);
    playback.fill_at(&mut [0.0], time);
    let resumed = submit.observed.pop().unwrap();
    assert_eq!((resumed.first, resumed.through), (full, full + 1));
    assert_eq!(submit.observed.pop(), None);
}

#[test]
fn adjacent_source_spans_keep_distinct_run_and_generation_tags() {
    use super::{Presented, Source};
    use kumochi_protocol::message::{RunId, SegmentId};
    use std::time::Duration;

    let (mut submit, mut playback) = pair(SOURCE_RATE, 1);
    let a = Source {
        run: RunId(5),
        generation: 3,
        segment: SegmentId(0),
        at: 100,
    };
    let b = Source {
        run: RunId(6),
        at: 101,
        ..a
    };
    let c = Source {
        generation: 4,
        at: 102,
        ..b
    };
    for source in [a, b, c] {
        submit
            .packet_for(&[16_384], PacketKind::End, Some(source))
            .unwrap();
    }
    let start = Duration::from_millis(10);
    playback.fill_at(&mut [0.0; 3], Some(start));
    let actual = std::array::from_fn(|_| submit.observed.pop());
    assert_eq!(
        actual,
        [
            Some(Presented {
                source: a,
                first: 100,
                through: 101,
                at: Some(start)
            }),
            Some(Presented {
                source: b,
                first: 101,
                through: 102,
                at: Some(start + Duration::from_nanos(41_666))
            }),
            Some(Presented {
                source: c,
                first: 102,
                through: 103,
                at: Some(start + Duration::from_nanos(83_333))
            }),
            None,
        ]
    );
    assert!(!submit.control.overflowed());
}

#[test]
fn canceling_one_backend_does_not_cancel_a_fallback_open() {
    use super::{BackendStream, Control};
    use std::sync::Arc;

    let control = Arc::new(Control::default());
    let (first, _) = super::pair(SOURCE_RATE, 2, Arc::clone(&control));
    drop(BackendStream((), Arc::clone(&first.shared.cancelled)));

    let (mut fallback, mut callback) = super::pair(SOURCE_RATE, 8, control);
    fallback.packet(&[16_384], PacketKind::End).unwrap();
    let mut out = [0.0; 8];
    callback.fill(&mut out);
    assert_eq!(
        out.map(f32::to_bits),
        [0.5f32.to_bits(), 0.5f32.to_bits(), 0, 0, 0, 0, 0, 0]
    );
}

pub(crate) use super::playback_fixture::FakePlayback;
#[cfg(target_os = "linux")]
#[test]
fn native_output_returns_unsupported_on_a_test_host() {
    assert_eq!(
        super::Output::open_default().unwrap_err(),
        OutputError::Unsupported
    );
}
