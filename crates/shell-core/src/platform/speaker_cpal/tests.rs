use super::{
    AtomicU8, BLOCKS, ErrorKind, MAX_FRAMES, MAX_RATE, OutputError, OutputEvent, PACKET_SAMPLES,
    PacketKind, SOURCE_RATE, pair,
};

#[test]
fn render_copies_prepared_stereo_frames_across_callback_boundaries_and_fills_silence() {
    let (mut submit, mut render) = pair(SOURCE_RATE, 2);
    submit.packet(&[i16::MIN, 16_384], PacketKind::End).unwrap();

    let mut first = [99.0; 2];
    render.fill(&mut first);
    assert_eq!(first.map(f32::to_bits), [-1.0, -1.0].map(f32::to_bits));
    assert_eq!(submit.buffers.free.len(), BLOCKS - 1);

    let mut second = [99.0; 4];
    render.fill(&mut second);
    assert_eq!(
        second.map(f32::to_bits),
        [0.5, 0.5, 0.0, 0.0].map(f32::to_bits)
    );
    assert_eq!(submit.buffers.free.len(), BLOCKS);

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
    let (mut submit, _) = pair(MAX_RATE, 2);
    submit
        .packet(&[16_384; PACKET_SAMPLES], PacketKind::More)
        .unwrap();
    submit
        .packet(&[16_384; PACKET_SAMPLES], PacketKind::End)
        .unwrap();
    let first = submit.buffers.ready.pop().unwrap();
    let final_block = submit.buffers.ready.pop().unwrap();
    assert_eq!(first.len() + final_block.len(), 15_360);
    assert_eq!(first.capacity(), MAX_FRAMES * 2);
    assert_eq!(final_block.capacity(), MAX_FRAMES * 2);
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
