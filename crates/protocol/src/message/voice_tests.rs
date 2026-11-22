//! Every voice bound at its limit and one past it, the largest frames, and redaction.

use super::{
    Cursor, MicPcm, OutputGeneration, PartState, SegmentId, SpeechPcm, Transcript, VoiceBound,
    VoiceView, check_rate,
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
    let transcript = |text| Transcript {
        run: RunId(1),
        text,
    };
    let limit = "é".repeat(8_192);
    assert_eq!(
        transcript(format!("{limit}a")).check(),
        Err(VoiceBound::Text)
    );
    assert_eq!(transcript(limit).check(), Ok(()));
    assert_eq!(transcript(String::new()).check(), Ok(()));
}

#[test]
fn the_largest_voice_frames_fit_their_pipes() {
    fn round_trip<T>(message: T, pipe: Pipe)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let bytes = frame::encode(&message, pipe).expect("fits the pipe's cap");
        assert!(bytes.len() - HEADER_LEN <= pipe.max_payload());
        assert_eq!(
            frame::read(&mut bytes.as_slice(), pipe).unwrap(),
            Some(message)
        );
    }

    let worst_mic = DataToBrain::MicPcm(MicPcm {
        gesture: GestureId(u32::MAX),
        seq: 2_999,
        rate: 192_000,
        last: false,
        samples: vec![i16::MIN; 3_840],
    });
    round_trip(worst_mic, Pipe::Data);
    let worst_speech = DataToShell::SpeechPcm(SpeechPcm {
        run: RunId(u64::MAX),
        segment: SegmentId(u32::MAX),
        output_generation: OutputGeneration(u32::MAX),
        seq: u32::MAX,
        samples: vec![i16::MIN; 19_200],
        last: Some(5_760_000),
    });
    round_trip(worst_speech, Pipe::Data);
    let worst_transcript = DataToShell::Transcript(Transcript {
        run: RunId(u64::MAX),
        text: "é".repeat(8_192),
    });
    round_trip(worst_transcript, Pipe::Data);
    let stopped = ControlToBrain::SpeechStopped {
        run: RunId(u64::MAX),
        cursor: Some(Cursor {
            segment: SegmentId(u32::MAX),
            sample: u32::MAX,
        }),
    };
    round_trip(stopped, Pipe::Control);
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
    use crate::tap::{self, Tapped};

    const PCM: i16 = 12_345;
    const TEXT: &str = "kumochi-voice-sentinel";

    fn check(message: &(impl Tapped + std::fmt::Debug), direction: Direction, sentinel: &[u8]) {
        let payload = frame::encode_payload(message).unwrap();
        let contains = |data: &[u8], needle: &[u8]| data.windows(needle.len()).any(|w| w == needle);
        assert!(
            contains(&payload, sentinel),
            "the wire carries the sentinel"
        );
        let mut record = Record::of(Pipe::Data, direction, message, payload.len()).unwrap();
        assert_eq!(record.payload, None);
        let metadata = format!("{} ({} bytes)", message.op(), payload.len());
        assert_eq!(record.describe(), metadata);
        let mut bytes = Vec::new();
        tap::write(&mut bytes, &record).unwrap();
        for forbidden in [
            sentinel,
            TEXT.as_bytes(),
            &PCM.to_le_bytes(),
            &PCM.to_be_bytes(),
        ] {
            assert!(!contains(&bytes, forbidden));
        }
        record.payload = Some(payload);
        assert_eq!(record.describe(), metadata);
        let mut forged = Vec::new();
        tap::write_header(&mut forged).unwrap();
        tap::write(&mut forged, &record).unwrap();
        let mut output = Vec::new();
        assert_eq!(tap::dump(&mut forged.as_slice(), &mut output).unwrap(), 1);
        let dump = String::from_utf8(output).unwrap();
        assert!(dump.contains(&metadata));
        let pcm_wire = postcard::to_allocvec(&PCM).unwrap();
        for output in [format!("{message:?}"), dump] {
            assert!(!output.contains("12345") && !output.contains(TEXT));
            for bytes in [TEXT.as_bytes(), &PCM.to_le_bytes(), &pcm_wire] {
                assert!(!output.contains(format!("{bytes:?}").trim_matches(['[', ']'])));
            }
        }
    }

    let pcm_wire = postcard::to_allocvec(&PCM).unwrap();
    check(
        &DataToBrain::MicPcm(MicPcm {
            samples: vec![PCM; 960],
            ..mic(48_000, 960, 0)
        }),
        Direction::ToBrain,
        &pcm_wire,
    );
    check(
        &DataToShell::SpeechPcm(SpeechPcm {
            samples: vec![PCM; 3],
            ..speech(3, Some(3))
        }),
        Direction::ToShell,
        &pcm_wire,
    );
    check(
        &DataToShell::Transcript(Transcript {
            run: RunId(1),
            text: TEXT.to_owned(),
        }),
        Direction::ToShell,
        TEXT.as_bytes(),
    );
}
