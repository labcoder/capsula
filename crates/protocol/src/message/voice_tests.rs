//! Every voice bound at its limit and one past it, the largest frames, and redaction.

use super::{
    Cursor, MAX_HOLD_CHUNKS, MAX_TRANSCRIPT_BYTES, MicPcm, OutputGeneration, PartState, SegmentId,
    SpeechPcm, Transcript, VoiceBound, VoiceView, check_rate,
};
use crate::frame::{self, FrameError, HEADER_LEN, Pipe};
use crate::message::ErrorKey;
use crate::message::{ControlToBrain, DataToBrain, DataToShell, GestureId, RunId};
use crate::tap::{Direction, Record};
use crate::voice::VoicePart;

/// The tags travel between builds, so moving a variant must fail here.
#[test]
fn voice_tags_are_pinned() {
    use crate::message::{SettingsOp, SttUnload};
    for (part, tag) in [(VoicePart::SpeechInput, 0), (VoicePart::SpokenReplies, 1)] {
        assert_eq!(
            postcard::to_allocvec(&part).expect("encodes"),
            [tag],
            "{part:?}"
        );
        assert_eq!(postcard::from_bytes::<VoicePart>(&[tag]), Ok(part));
    }
    for (state, tag) in [
        (PartState::NotInstalled, 0),
        (PartState::Verifying, 1),
        (PartState::Downloading { done: 0, total: 0 }, 2),
        (PartState::Loading, 3),
        (PartState::Ready, 4),
        (PartState::Unloaded, 5),
        (
            PartState::Failed {
                key: ErrorKey::VoiceLoad,
                installed: false,
            },
            6,
        ),
        (PartState::Unavailable, 7),
    ] {
        let bytes = postcard::to_allocvec(&state).expect("encodes");
        assert_eq!(bytes[0], tag, "{state:?}");
        assert_eq!(postcard::from_bytes::<PartState>(&bytes), Ok(state));
    }
    for (unload, tag) in [
        (SttUnload::Kept, 0),
        (SttUnload::After15Min, 1),
        (SttUnload::After1Hour, 2),
    ] {
        assert_eq!(
            postcard::to_allocvec(&SettingsOp::SetSttUnload(unload)).expect("encodes"),
            [8, tag],
            "{unload:?}"
        );
    }
    assert!(postcard::from_bytes::<VoicePart>(&[0x7f]).is_err());
    assert!(postcard::from_bytes::<PartState>(&[0x7f]).is_err());
    let downloading = PartState::Downloading {
        done: u64::MAX - 1,
        total: u64::MAX,
    };
    for view in [
        VoiceView {
            speech_input: downloading,
            spoken_replies: PartState::Failed {
                key: ErrorKey::VoiceCorrupt,
                installed: false,
            },
        },
        VoiceView {
            speech_input: PartState::Failed {
                key: ErrorKey::VoiceLoad,
                installed: true,
            },
            spoken_replies: downloading,
        },
    ] {
        let bytes = postcard::to_allocvec(&view).expect("encodes");
        assert_eq!(postcard::from_bytes::<VoiceView>(&bytes), Ok(view));
    }
}

fn mic(rate: u32, len: usize, seq: u32) -> MicPcm {
    MicPcm {
        gesture: GestureId(1),
        seq,
        rate,
        last: false,
        samples: vec![0; len],
    }
}

fn speech(len: usize, last: Option<u32>) -> SpeechPcm {
    SpeechPcm {
        run: RunId(1),
        segment: SegmentId(0),
        output_generation: OutputGeneration(1),
        seq: 0,
        samples: vec![0; len],
        last,
    }
}

#[test]
fn rates_from_8_to_192_khz_are_allowed() {
    assert_eq!(check_rate(7_999), Err(VoiceBound::Rate));
    assert_eq!(check_rate(8_000), Ok(()));
    assert_eq!(check_rate(192_000), Ok(()));
    assert_eq!(check_rate(192_001), Err(VoiceBound::Rate));
    assert_eq!(mic(7_999, 159, 0).check(), Err(VoiceBound::Rate));
    assert_eq!(mic(192_001, 3_840, 0).check(), Err(VoiceBound::Rate));
    assert_eq!(speech(1, None).check(192_001), Err(VoiceBound::Rate));
}

#[test]
fn a_mic_chunk_is_exactly_20_ms_unless_it_is_the_last() {
    for (len, last, expected) in [
        (960, false, Ok(())),
        (959, false, Err(VoiceBound::Samples)),
        (961, false, Err(VoiceBound::Samples)),
        (0, false, Err(VoiceBound::Samples)),
        (1, true, Ok(())),
        (960, true, Ok(())),
        (961, true, Err(VoiceBound::Samples)),
        (0, true, Err(VoiceBound::Samples)),
    ] {
        assert_eq!(
            MicPcm {
                last,
                ..mic(48_000, len, 0)
            }
            .check(),
            expected,
            "{len} last={last}"
        );
    }
    assert_eq!(mic(192_000, 3_840, 0).check(), Ok(()));
    assert_eq!(mic(8_000, 160, 0).check(), Ok(()));
}

#[test]
fn a_hold_sends_at_most_3000_chunks() {
    assert_eq!(
        MicPcm {
            last: true,
            ..mic(48_000, 960, 2_999)
        }
        .check(),
        Ok(())
    );
    assert_eq!(
        MicPcm {
            last: true,
            ..mic(48_000, 960, 3_000)
        }
        .check(),
        Err(VoiceBound::Chunks)
    );
}

#[test]
fn a_speech_chunk_holds_up_to_100_ms_and_a_segment_up_to_30_s() {
    for (len, expected) in [
        (1, Ok(())),
        (2_400, Ok(())),
        (2_401, Err(VoiceBound::Samples)),
        (0, Err(VoiceBound::Samples)),
    ] {
        assert_eq!(speech(len, None).check(24_000), expected, "{len}");
    }
    assert_eq!(speech(0, Some(0)).check(24_000), Ok(()));
    assert_eq!(speech(0, Some(720_000)).check(24_000), Ok(()));
    assert_eq!(speech(1, Some(0)).check(24_000), Err(VoiceBound::Segment));
    assert_eq!(speech(1, Some(1)).check(24_000), Ok(()));
    assert_eq!(
        speech(2_400, Some(2_399)).check(24_000),
        Err(VoiceBound::Segment)
    );
    assert_eq!(
        speech(2_401, Some(2_401)).check(24_000),
        Err(VoiceBound::Samples)
    );
    assert_eq!(speech(1, Some(720_000)).check(24_000), Ok(()));
    assert_eq!(
        speech(1, Some(720_001)).check(24_000),
        Err(VoiceBound::Segment)
    );
    assert_eq!(speech(19_200, Some(5_760_000)).check(192_000), Ok(()));
}

#[test]
fn a_transcript_is_at_most_16_kib() {
    let transcript = |len| Transcript {
        run: RunId(1),
        text: "a".repeat(len),
    };
    assert_eq!(transcript(MAX_TRANSCRIPT_BYTES).check(), Ok(()));
    assert_eq!(transcript(0).check(), Ok(()));
    assert_eq!(
        transcript(MAX_TRANSCRIPT_BYTES + 1).check(),
        Err(VoiceBound::Text)
    );
}

fn payload<T: serde::Serialize>(message: &T, pipe: Pipe) -> usize {
    frame::encode(message, pipe)
        .expect("fits the pipe's cap")
        .len()
        - HEADER_LEN
}

#[test]
fn the_largest_voice_frames_fit_their_pipes() {
    let worst_mic = DataToBrain::MicPcm(MicPcm {
        gesture: GestureId(u32::MAX),
        seq: MAX_HOLD_CHUNKS - 1,
        rate: 192_000,
        last: false,
        samples: vec![i16::MIN; 3_840],
    });
    // Tag, gesture, a 2-byte seq, a 3-byte rate, last, a 2-byte length, 3 bytes a sample.
    assert_eq!(
        payload(&worst_mic, Pipe::Data),
        1 + 5 + 2 + 3 + 1 + 2 + 3_840 * 3
    );
    let worst_speech = DataToShell::SpeechPcm(SpeechPcm {
        run: RunId(u64::MAX),
        segment: SegmentId(u32::MAX),
        output_generation: OutputGeneration(u32::MAX),
        seq: u32::MAX,
        samples: vec![i16::MIN; 19_200],
        last: Some(5_760_000),
    });
    // Tag, run, segment, generation and seq at their widest, a 3-byte length, 3 bytes a
    // sample, then Some and a 4-byte total.
    assert_eq!(
        payload(&worst_speech, Pipe::Data),
        1 + 10 + 5 + 5 + 5 + 3 + 19_200 * 3 + 1 + 4
    );
    let worst_transcript = DataToShell::Transcript(Transcript {
        run: RunId(u64::MAX),
        text: "a".repeat(MAX_TRANSCRIPT_BYTES),
    });
    assert_eq!(
        payload(&worst_transcript, Pipe::Data),
        1 + 10 + 3 + MAX_TRANSCRIPT_BYTES
    );
    let stopped = ControlToBrain::SpeechStopped {
        run: RunId(u64::MAX),
        cursor: Some(Cursor {
            segment: SegmentId(u32::MAX),
            sample: u32::MAX,
        }),
    };
    assert_eq!(payload(&stopped, Pipe::Control), 1 + 10 + 1 + 5 + 5);
}

#[test]
fn truncated_and_mistyped_voice_frames_are_refused() {
    let message = DataToBrain::MicPcm(mic(48_000, 960, 0));
    let frame = frame::encode(&message, Pipe::Data).expect("encodes");
    let body = &frame[HEADER_LEN..];
    assert_eq!(frame::decode::<DataToBrain>(body).ok(), Some(message));
    assert!(matches!(
        frame::decode::<DataToBrain>(&body[..body.len() - 1]),
        Err(FrameError::Malformed(
            postcard::Error::DeserializeUnexpectedEnd
        ))
    ));
    let mut mistyped = body.to_vec();
    mistyped[0] = 0x7f;
    assert!(matches!(
        frame::decode::<DataToBrain>(&mistyped),
        Err(FrameError::Malformed(postcard::Error::SerdeDeCustom))
    ));
}

#[test]
fn debug_and_the_tap_show_no_audio_or_text() {
    let marker = "kumochi-voice-sentinel";
    let transcript = |text: &str| {
        DataToShell::Transcript(Transcript {
            run: RunId(1),
            text: text.to_owned(),
        })
    };
    let sample: i16 = 12_345;
    let other_sample: i16 = -23_456;
    let messages: [(String, Record); 3] = [
        {
            let pcm = |value| {
                DataToBrain::MicPcm(MicPcm {
                    samples: vec![value; 960],
                    ..mic(48_000, 960, 0)
                })
            };
            let message = pcm(sample);
            let debug = format!("{message:?}");
            assert!(
                debug == format!("{:?}", pcm(other_sample)),
                "microphone samples affected voice Debug"
            );
            let record = Record::of(Pipe::Data, Direction::ToBrain, &message, 10).expect("records");
            (debug, record)
        },
        {
            let pcm = |value| {
                DataToShell::SpeechPcm(SpeechPcm {
                    samples: vec![value; 3],
                    ..speech(3, Some(3))
                })
            };
            let message = pcm(sample);
            let debug = format!("{message:?}");
            assert!(
                debug == format!("{:?}", pcm(other_sample)),
                "speech samples affected voice Debug"
            );
            let record = Record::of(Pipe::Data, Direction::ToShell, &message, 10).expect("records");
            (debug, record)
        },
        {
            let message = transcript(marker);
            let other = transcript("another-spoken-message");
            let debug = format!("{message:?}");
            assert!(
                debug == format!("{other:?}"),
                "transcript affected voice Debug"
            );
            let record = Record::of(Pipe::Data, Direction::ToShell, &message, 10).expect("records");
            (debug, record)
        },
    ];
    let bytes = format!("{:?}", marker.as_bytes());
    let bytes = &bytes[1..bytes.len() - 1];
    for (debug, record) in messages {
        assert!(
            !debug.contains("12345") && !debug.contains(marker),
            "voice Debug contains a private value"
        );
        assert!(!debug.contains(bytes), "voice Debug contains text bytes");
        // 12,345 as little-endian bytes, the way a byte list of the PCM would print.
        assert!(
            !debug.contains("57, 48"),
            "voice Debug contains sample bytes"
        );
        assert!(
            record.payload.is_none(),
            "voice recording contains a payload"
        );
    }
}
