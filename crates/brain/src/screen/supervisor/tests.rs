use super::*;

fn pending(
    id: u64,
) -> (
    BTreeMap<u64, Pending>,
    oneshot::Receiver<Result<ReplyBody, ScreenError>>,
) {
    let (answer, receive) = oneshot::channel();
    (
        BTreeMap::from([(
            id,
            Pending {
                until: Instant::now() + REQUEST_DEADLINE,
                answer,
                assembly: Assembly::default(),
            },
        )]),
        receive,
    )
}

#[test]
fn identities_completion_trailing_and_faults() {
    let (mut requests, mut receive) = pending(1);
    assert_eq!(
        accept(
            &mut requests,
            super::super::wire::reply(2, &ReplyBody::Pong).expect("pong")
        ),
        Err(ScreenError::WorkerLost)
    );
    assert_eq!(
        accept(
            &mut requests,
            super::super::wire::reply(1, &ReplyBody::Pong).expect("pong")
        ),
        Ok(())
    );
    assert_eq!(receive.try_recv(), Ok(Ok(ReplyBody::Pong)));
    assert_eq!(
        accept(
            &mut requests,
            super::super::wire::reply(1, &ReplyBody::Pong).expect("after last")
        ),
        Err(ScreenError::WorkerLost)
    );
    let (mut requests, _) = pending(1);
    let mut trailing = super::super::wire::reply(1, &ReplyBody::Pong).expect("pong");
    trailing.body.push(0);
    assert_eq!(
        accept(&mut requests, trailing),
        Err(ScreenError::WorkerLost)
    );
    let (mut requests, mut receive) = pending(1);
    let encoded = frame::encode(&ReplyBody::Fault(-7), Pipe::Data).expect("fault");
    assert_eq!(
        accept(
            &mut requests,
            Reply {
                id: 1,
                chunk: 0,
                last: true,
                body: encoded[4..].to_vec()
            }
        ),
        Ok(())
    );
    assert_eq!(receive.try_recv(), Ok(Err(ScreenError::Fault(-7))));
    let (mut requests, _) = pending(1);
    assert_eq!(
        accept(
            &mut requests,
            Reply {
                id: 1,
                chunk: 0,
                last: false,
                body: vec![0]
            }
        ),
        Ok(())
    );
    assert_eq!(
        accept(
            &mut requests,
            Reply {
                id: 2,
                chunk: 1,
                last: true,
                body: vec![0]
            }
        ),
        Err(ScreenError::WorkerLost)
    );
}

#[tokio::test]
async fn request_queue_accepts_sixteen_and_refuses_seventeen() {
    use std::future::Future as _;
    use std::task::{Context, Poll, Waker};

    let (commands, mut receive) = mpsc::channel(QUEUE);
    let (losses, _) = broadcast::channel(QUEUE);
    let handle = ScreenHandle {
        commands,
        losses,
        deadline: REQUEST_DEADLINE,
    };
    let mut requests = Vec::new();
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..16 {
        let mut request = Box::pin(handle.request(RequestBody::Ping));
        assert_eq!(request.as_mut().poll(&mut context), Poll::Pending);
        requests.push(request);
    }
    assert_eq!(
        handle.request(RequestBody::Ping).await,
        Err(ScreenError::WorkerLost)
    );
    assert_eq!(receive.len(), 16);
    for _ in 0..16 {
        complete(
            receive.try_recv().expect("accepted request").answer,
            Ok(ReplyBody::Pong),
        );
    }
    for request in requests {
        assert_eq!(request.await, Ok(ReplyBody::Pong));
    }
}

#[tokio::test(start_paused = true)]
async fn a_request_queued_behind_a_held_respawn_keeps_its_deadline() {
    use std::future::Future as _;
    use std::task::{Context, Poll, Waker};
    let (commands, mut receive) = mpsc::channel(QUEUE);
    let (losses, _) = broadcast::channel(QUEUE);
    let handle = ScreenHandle {
        commands,
        losses,
        deadline: REQUEST_DEADLINE,
    };
    let mut request = Box::pin(handle.request(RequestBody::Ping));
    assert_eq!(
        request
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    );
    assert_eq!(receive.len(), 1);
    tokio::time::advance(Duration::from_millis(1499)).await;
    assert_eq!(
        request
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    );
    tokio::time::advance(Duration::from_millis(2)).await;
    assert_eq!(request.await, Err(ScreenError::WorkerLost));
    assert!(
        receive
            .try_recv()
            .expect("queued while respawning")
            .answer
            .is_closed()
    );
}

#[tokio::test(start_paused = true)]
async fn third_fourth_fifth_loss_and_window_boundary() {
    let mut history = VecDeque::new();
    let now = Instant::now();
    assert_eq!(backoff(&mut history, now), Duration::ZERO);
    assert_eq!(backoff(&mut history, now), Duration::ZERO);
    assert_eq!(backoff(&mut history, now), Duration::from_millis(200));
    assert_eq!(backoff(&mut history, now), Duration::from_secs(1));
    assert_eq!(backoff(&mut history, now), Duration::from_secs(5));
    tokio::time::advance(
        WINDOW
            .checked_sub(Duration::from_nanos(1))
            .expect("one ns under window"),
    )
    .await;
    assert_eq!(
        backoff(&mut history, Instant::now()),
        Duration::from_secs(5)
    );
    tokio::time::advance(Duration::from_nanos(1)).await;
    assert_eq!(backoff(&mut history, Instant::now()), Duration::ZERO);
}

#[tokio::test]
async fn capture_accepts_only_its_own_frame_and_passes_refusals_through() {
    use kumochi_protocol::geometry::PhysicalRect;
    use kumochi_protocol::message::RunId;

    use super::super::wire::CaptureId;

    let (commands, mut receive) = mpsc::channel(QUEUE);
    let (losses, _) = broadcast::channel(QUEUE);
    let handle = ScreenHandle {
        commands,
        losses,
        deadline: REQUEST_DEADLINE,
    };
    let display = DisplayRef(PhysicalRect {
        origin: [0, 0],
        size: [2560, 1440],
    });
    let meta = |capture, display| FrameMeta {
        capture: CaptureId(capture),
        display,
        dpi: 96,
        acquired_after_ms: 55,
    };
    let other = PhysicalRect {
        origin: [2560, 0],
        size: [2560, 1440],
    };
    for (reply, expected) in [
        (
            Ok(ReplyBody::Captured(meta(7, display.0))),
            Ok(meta(7, display.0)),
        ),
        (
            Ok(ReplyBody::Captured(meta(8, display.0))),
            Err(CaptureError::Screen(ScreenError::WorkerLost)),
        ),
        (
            Ok(ReplyBody::Captured(meta(7, other))),
            Err(CaptureError::Screen(ScreenError::WorkerLost)),
        ),
        (
            Ok(ReplyBody::Refused(Refusal::Stale)),
            Err(CaptureError::Refused(Refusal::Stale)),
        ),
        (
            Ok(ReplyBody::Pong),
            Err(CaptureError::Screen(ScreenError::WorkerLost)),
        ),
        (
            Err(ScreenError::Fault(-9)),
            Err(CaptureError::Screen(ScreenError::Fault(-9))),
        ),
    ] {
        let ticket = TicketWire::new(RunId(1), CaptureId(7), 1, [0; 16], 2_000);
        let capture = handle.capture(ticket, display, Exclusion::KumochiWindows);
        let answer = async {
            let command = receive.recv().await.expect("queued capture");
            assert!(matches!(
                command.body,
                RequestBody::Capture {
                    exclude: Exclusion::KumochiWindows,
                    ..
                }
            ));
            complete(command.answer, reply);
        };
        let (result, ()) = tokio::join!(capture, answer);
        assert_eq!(result, expected);
    }
}
