use super::super::*;
use crate::screen::{ElementTree, Unavailable};

#[tokio::test(start_paused = true)]
async fn elements_waits_end_on_cancellation_eof_and_deadline() {
    for end in ["cancel", "eof", "deadline"] {
        let (inner, mut receive) = supervise::detached::<Screen>();
        let handle = ScreenHandle {
            inner,
            deadline: Duration::from_millis(100),
        };
        let mut pending = Box::pin(handle.elements(CaptureId(7), ElementTarget::Foreground));
        let (_, answer) = tokio::select! {
            result = &mut pending => panic!("unexpected early result: {result:?}"),
            request = receive.recv() => request.unwrap(),
        };
        match end {
            "cancel" => {
                drop(pending);
                assert!(answer.is_closed());
            }
            "eof" => {
                drop(answer);
                assert_eq!(
                    pending.await,
                    Err(CaptureError::Screen(ScreenError::WorkerLost))
                );
            }
            "deadline" => {
                tokio::time::advance(Duration::from_millis(100)).await;
                assert_eq!(
                    pending.await,
                    Err(CaptureError::Screen(ScreenError::WorkerLost))
                );
                assert!(answer.is_closed());
            }
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
async fn elements_accept_only_the_requested_capture_and_map_refusals() {
    let (inner, mut receive) = supervise::detached::<Screen>();
    let handle = ScreenHandle {
        inner,
        deadline: REQUEST_DEADLINE,
    };
    for (reply, expected) in [
        (
            Ok(ReplyBody::Elements(ElementTree::unavailable(
                CaptureId(7),
                Unavailable::NoWindow,
            ))),
            Ok(ElementTree::unavailable(
                CaptureId(7),
                Unavailable::NoWindow,
            )),
        ),
        (
            Ok(ReplyBody::Elements(ElementTree::unavailable(
                CaptureId(8),
                Unavailable::NoWindow,
            ))),
            Err(CaptureError::Screen(ScreenError::WorkerLost)),
        ),
        (
            Ok(ReplyBody::Refused(Refusal::Expired)),
            Err(CaptureError::Refused(Refusal::Expired)),
        ),
        (
            Ok(ReplyBody::Pong),
            Err(CaptureError::Screen(ScreenError::WorkerLost)),
        ),
        (
            Ok(ReplyBody::Fault(-7)),
            Err(CaptureError::Screen(ScreenError::Fault(-7))),
        ),
        (
            Err(WorkerLost),
            Err(CaptureError::Screen(ScreenError::WorkerLost)),
        ),
    ] {
        let request = handle.elements(CaptureId(7), ElementTarget::Foreground);
        let answer = async {
            let (body, send) = receive.recv().await.unwrap();
            let RequestBody::Elements { capture, target } = body else {
                panic!("not an Elements request")
            };
            assert_eq!(capture, CaptureId(7));
            assert_eq!(target, ElementTarget::Foreground);
            assert_eq!(send.send(reply), Ok(()));
        };
        let (actual, ()) = tokio::join!(request, answer);
        assert_eq!(actual, expected);
    }
}
