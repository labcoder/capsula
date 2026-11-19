use super::{
    AtomicU8, BLOCKS, ErrorKind, MAX_FRAMES, MAX_RATE, OutputError, OutputEvent, PACKET_SAMPLES,
    PacketKind, ReadyBlock, SOURCE_RATE, pair,
};

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
    submit
        .packet(&[16_384; PACKET_SAMPLES], PacketKind::End)
        .unwrap();
    let first = render.ready.pop().unwrap();
    let final_block = render.ready.pop().unwrap();
    assert_eq!(
        first.pcm.samples.len() + final_block.pcm.samples.len(),
        15_360
    );
    assert_eq!(first.pcm.samples.capacity(), MAX_FRAMES * 2);
    assert_eq!(final_block.pcm.samples.capacity(), MAX_FRAMES * 2);
    assert_eq!(submit.mono.capacity(), MAX_FRAMES);
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
    retained_wrapper(DropPath::Normal);
}

#[test]
fn a_callback_retained_past_wrapper_unwind_cannot_play() {
    retained_wrapper(DropPath::Unwind);
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
