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
        accept(&mut requests, super::super::wire::pong(2).expect("pong")),
        Err(ScreenError::WorkerLost)
    );
    assert_eq!(
        accept(&mut requests, super::super::wire::pong(1).expect("pong")),
        Ok(())
    );
    assert_eq!(receive.try_recv(), Ok(Ok(ReplyBody::Pong)));
    assert_eq!(
        accept(
            &mut requests,
            super::super::wire::pong(1).expect("after last")
        ),
        Err(ScreenError::WorkerLost)
    );
    let (mut requests, _) = pending(1);
    let mut trailing = super::super::wire::pong(1).expect("pong");
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
