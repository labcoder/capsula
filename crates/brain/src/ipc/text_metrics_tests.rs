use super::{DataToBrain, FrameError, Pipe, SessionError, TextMetricsV1, frame, read_text_metrics};
use tokio::io::AsyncWriteExt as _;
use tokio::time::Instant;

fn deadline() -> Instant {
    Instant::now() + super::HANDSHAKE_TIMEOUT
}

#[tokio::test]
async fn startup_checks_the_304_byte_limit_before_reading_the_body() {
    assert!(matches!(
        read_text_metrics(&mut 305_u32.to_le_bytes().as_slice(), deadline()).await,
        Err(SessionError::Frame(FrameError::TooLarge {
            len: 305,
            max: 304
        }))
    ));
    assert!(matches!(
        read_text_metrics(&mut 304_u32.to_le_bytes().as_slice(), deadline()).await,
        Err(SessionError::Frame(FrameError::Truncated))
    ));
}

#[tokio::test]
async fn explicit_absence_and_invalid_metrics_select_fallback_but_missing_or_bad_frames_fail() {
    let valid = TextMetricsV1 {
        advances: [8192; 95],
        line_height: 4096,
        range_maxima: [8192; 3],
    };
    let mut invalid = valid;
    invalid.advances[94] = 63;
    for (sent, want) in [
        (None, None),
        (Some(invalid), None),
        (Some(valid), Some(valid)),
    ] {
        let bytes = frame::encode(&DataToBrain::TextMetrics(sent), Pipe::Data).unwrap();
        assert_eq!(
            read_text_metrics(&mut bytes.as_slice(), deadline())
                .await
                .unwrap(),
            want
        );
    }
    assert!(matches!(
        read_text_metrics(&mut [].as_slice(), deadline()).await,
        Err(SessionError::ClosedEarly)
    ));
    assert!(matches!(
        read_text_metrics(&mut [1, 0, 0, 0, 255].as_slice(), deadline()).await,
        Err(SessionError::Frame(FrameError::Malformed(_)))
    ));
    let wrong = frame::encode(&DataToBrain::Credit { bytes: 1 }, Pipe::Data).unwrap();
    assert!(matches!(
        read_text_metrics(&mut wrong.as_slice(), deadline()).await,
        Err(SessionError::Protocol("expected startup TextMetrics"))
    ));
}

#[tokio::test(start_paused = true)]
async fn a_partial_startup_frame_cannot_extend_the_second_leg_deadline() {
    let (mut pipe, mut shell) = tokio::io::duplex(16);
    shell.write_all(&[2, 0, 0, 0]).await.unwrap();
    let deadline = deadline();
    tokio::time::advance(super::HANDSHAKE_TIMEOUT / 2).await;
    assert!(matches!(
        read_text_metrics(&mut pipe, deadline).await,
        Err(SessionError::Timeout)
    ));
    assert!(Instant::now() < deadline + super::HANDSHAKE_TIMEOUT / 2);
}
