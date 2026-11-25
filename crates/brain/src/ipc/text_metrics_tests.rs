use std::path::PathBuf;
use std::pin::pin;
use std::task::{Context, Waker};

use kumochi_protocol::message::{CAPABILITY_LEN, IssueSession};
use kumochi_protocol::{BUILD_ID, Secret};
use tokio::io::AsyncWriteExt as _;
use tokio::time::{Instant, sleep_until};

use super::{
    AudioSession, DataToBrain, FrameError, HANDSHAKE_TIMEOUT, Hello, Identity, Incarnation, Pipe,
    SessionError, Tap, TextMetricsV1, frame, read_startup_data, read_text_metrics,
};

const VALID_METRICS: TextMetricsV1 = TextMetricsV1 {
    advances: [8192; 95],
    line_height: 4096,
    range_maxima: [8192; 3],
};

fn deadline() -> Instant {
    Instant::now() + HANDSHAKE_TIMEOUT
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
    let valid = VALID_METRICS;
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
    tokio::time::advance(HANDSHAKE_TIMEOUT / 2).await;
    assert!(matches!(
        read_text_metrics(&mut pipe, deadline).await,
        Err(SessionError::Timeout)
    ));
    assert!(Instant::now() < deadline + HANDSHAKE_TIMEOUT / 2);
}

#[tokio::test(start_paused = true)]
async fn data_connect_hello_and_metrics_share_one_budget_in_both_modes() {
    let identity = Identity {
        capability: Secret::new([1; CAPABILITY_LEN]),
        incarnation: Incarnation(1),
        shell_image: PathBuf::new(),
    };
    let session = IssueSession {
        incarnation: identity.incarnation,
        epoch: 1,
    };
    let metrics =
        frame::encode(&DataToBrain::TextMetrics(Some(VALID_METRICS)), Pipe::Data).unwrap();
    let tick = HANDSHAKE_TIMEOUT / 5;
    for audio in [AudioSession::Legacy, AudioSession::QueuedRequired] {
        let hello = Hello {
            build_id: BUILD_ID.to_owned(),
            capability: Secret::new([1; CAPABILITY_LEN]),
        };
        let (hello, expected) = match audio {
            AudioSession::Legacy => (DataToBrain::Hello(hello), None),
            AudioSession::QueuedRequired => {
                (DataToBrain::QueuedHello(hello, session), Some(session))
            }
        };
        let hello = frame::encode(&hello, Pipe::Data).unwrap();
        for (connect_at, hello_at, metrics_at, on_time) in [
            (6, 7, 8, false),
            (2, 6, 7, false),
            (2, 4, 6, false),
            (2, 3, 4, true),
        ] {
            let (mut pipe, mut shell) = tokio::io::duplex(hello.len() + metrics.len());
            let mut tap = Tap::default();
            let start = Instant::now();
            let startup = async {
                let result = read_startup_data(
                    &identity,
                    expected,
                    &mut tap,
                    start + HANDSHAKE_TIMEOUT,
                    async {
                        sleep_until(start + tick * connect_at).await;
                        Ok(&mut pipe)
                    },
                    |_| Ok(()),
                )
                .await;
                (result, Instant::now())
            };
            let peer = async {
                sleep_until(start + tick * hello_at).await;
                shell.write_all(&hello).await.unwrap();
                sleep_until(start + tick * metrics_at).await;
                shell.write_all(&metrics).await.unwrap();
            };
            let ((result, finished), ()) = tokio::join!(startup, peer);
            if on_time {
                assert_eq!(result.unwrap(), ((), Some(VALID_METRICS)), "{audio:?}");
            } else {
                assert!(
                    matches!(result, Err(SessionError::Timeout)) && finished < start + tick * 6,
                    "{audio:?}, connect={connect_at}, hello={hello_at}, metrics={metrics_at}: {result:?}"
                );
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_complete_metrics_frame_ready_at_the_deadline_is_rejected() {
    let bytes = frame::encode(&DataToBrain::TextMetrics(Some(VALID_METRICS)), Pipe::Data).unwrap();
    let (mut pipe, mut shell) = tokio::io::duplex(bytes.len());
    let until = deadline();
    let mut read = pin!(read_text_metrics(&mut pipe, until));
    assert!(
        read.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    tokio::time::advance(HANDSHAKE_TIMEOUT).await;
    shell.write_all(&bytes).await.unwrap();
    assert_eq!(Instant::now(), until);
    assert!(matches!(read.await, Err(SessionError::Timeout)));
}
