use super::*;

#[tokio::test(start_paused = true)]
async fn a_timed_out_queued_request_stays_pending_until_definitive_discard() {
    let (handle, mut commands, _intents) = handle();
    let (track, receipt) = watch::channel(Dispatch::Pending);
    let until = Instant::now() + Duration::from_secs(1);
    let mut request = Box::pin(handle.request_tracked(vec![3], until, track));
    assert!(
        request
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let command = commands.recv().await.expect("queued command");
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(request.await, Err(WorkerLost::Lost));
    assert_eq!(
        *receipt.borrow(),
        Dispatch::Pending,
        "timeout is not discard"
    );
    drop(command);
    assert_eq!(*receipt.borrow(), Dispatch::NotDispatched);
}

#[tokio::test(start_paused = true)]
async fn a_command_already_selected_can_dispatch_after_its_caller_times_out() {
    let (handle, mut commands, _intents) = handle();
    let (track, receipt) = watch::channel(Dispatch::Pending);
    let until = Instant::now() + Duration::from_secs(1);
    let mut request = Box::pin(handle.request_tracked(vec![5], until, track));
    assert!(
        request
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let mut command = commands.recv().await.expect("selected before the deadline");
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(request.await, Err(WorkerLost::Lost));
    assert_eq!(*receipt.borrow(), Dispatch::Pending);
    command.answer.dispatched(Generation(7));
    assert_eq!(*receipt.borrow(), Dispatch::Dispatched(Generation(7)));
    drop(command);
    assert_eq!(
        *receipt.borrow(),
        Dispatch::Dispatched(Generation(7)),
        "discard cannot undo dispatch"
    );
}

#[tokio::test]
async fn dropping_only_the_caller_does_not_discard_the_queued_command() {
    let (handle, mut commands, _intents) = handle();
    let (track, receipt) = watch::channel(Dispatch::Pending);
    let mut request = Box::pin(handle.request_tracked(vec![1], Instant::now() + WINDOW, track));
    assert!(
        request
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(request);
    assert_eq!(*receipt.borrow(), Dispatch::Pending);
    let command = commands.recv().await.expect("still queued");
    assert!(command.answer.is_closed());
    drop(command);
    assert_eq!(*receipt.borrow(), Dispatch::NotDispatched);
}

#[tokio::test]
async fn dispatch_is_visible_before_the_writer_can_receive_the_body() {
    let (handle, mut commands, mut intents) = handle();
    let (track, receipt) = watch::channel(Dispatch::Pending);
    let until = Instant::now() + WINDOW;
    let request = handle.request_tracked(vec![9], until, track);
    let (writes, written) = blocking::sync_channel(QUEUE);
    let (frames_in, mut reads) = mpsc::channel(QUEUE);
    let mut id = 0;
    let serving = serve::<Test>(
        &mut reads,
        &writes,
        &mut commands,
        &mut id,
        Generation(4),
        &mut intents,
    );
    let client = async {
        tokio::pin!(request);
        assert!(
            request
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let body = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(body) = written.try_recv() {
                    break body;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the writer receives");
        assert_eq!(body, (1, vec![9]));
        assert_eq!(*receipt.borrow(), Dispatch::Dispatched(Generation(4)));
        frames_in.send(None).await.expect("end worker");
        assert_eq!(request.await, Err(WorkerLost::Lost));
        assert_eq!(
            *receipt.borrow(),
            Dispatch::Dispatched(Generation(4)),
            "failure is not release"
        );
    };
    let (ended, ()) = tokio::join!(serving, client);
    assert_eq!(ended, Some(LossReason::Ended));
}

#[derive(Clone, Copy)]
enum Writer {
    Full,
    Closed,
}

async fn writer_refusal_stays_dispatched(condition: Writer) {
    let (handle, mut commands, mut intents) = handle();
    let (track, receipt) = watch::channel(Dispatch::Pending);
    let (writes, written) = blocking::sync_channel(0);
    let retained = if matches!(condition, Writer::Closed) {
        drop(written);
        None
    } else {
        Some(written)
    };
    let (_frames, mut reads) = mpsc::channel(QUEUE);
    let mut id = 0;
    let request = handle.request_tracked(vec![9], Instant::now() + WINDOW, track);
    let serving = serve::<Test>(
        &mut reads,
        &writes,
        &mut commands,
        &mut id,
        Generation(4),
        &mut intents,
    );
    let (answer, ended) = tokio::join!(request, serving);
    assert_eq!(answer, Err(WorkerLost::Lost));
    assert_eq!(ended, Some(LossReason::Unwritable));
    assert_eq!(*receipt.borrow(), Dispatch::Dispatched(Generation(4)));
    drop(retained);
}

#[tokio::test]
async fn a_full_writer_queue_conservatively_keeps_the_dispatched_generation() {
    for condition in [Writer::Full, Writer::Closed] {
        writer_refusal_stays_dispatched(condition).await;
    }
}

#[tokio::test]
async fn queue_and_fatal_admission_refusals_are_definitive_no_dispatch() {
    let (handle, _commands, _intents) = handle();
    for _ in 0..QUEUE {
        let mut request = Box::pin(handle.request(vec![1], Instant::now() + WINDOW));
        assert!(
            request
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
    }
    let (track, receipt) = watch::channel(Dispatch::Pending);
    assert_eq!(
        handle
            .request_tracked(vec![2], Instant::now() + WINDOW, track)
            .await,
        Err(WorkerLost::Lost)
    );
    assert_eq!(*receipt.borrow(), Dispatch::NotDispatched);
    handle.notices.fatal.store(true, Ordering::Release);
    let (track, receipt) = watch::channel(Dispatch::Pending);
    assert_eq!(
        handle
            .request_tracked(vec![3], Instant::now() + WINDOW, track)
            .await,
        Err(WorkerLost::ExitUnconfirmed)
    );
    assert_eq!(*receipt.borrow(), Dispatch::NotDispatched);
}

#[tokio::test]
async fn the_supervisor_definitively_discards_an_expired_queued_body() {
    let (handle, mut commands, mut intents) = handle();
    let (track, mut receipt) = watch::channel(Dispatch::Pending);
    let (writes, written) = blocking::sync_channel(QUEUE);
    let (frames, mut reads) = mpsc::channel(QUEUE);
    let mut id = 0;
    let client = async {
        assert_eq!(
            handle.request_tracked(vec![9], Instant::now(), track).await,
            Err(WorkerLost::Lost)
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while *receipt.borrow() == Dispatch::Pending {
                receipt.changed().await.expect("definitive disposition");
            }
        })
        .await
        .expect("supervisor discards");
        assert_eq!(*receipt.borrow(), Dispatch::NotDispatched);
        assert!(matches!(
            written.try_recv(),
            Err(blocking::TryRecvError::Empty)
        ));
        frames.send(None).await.expect("end worker");
    };
    let (ended, ()) = tokio::join!(
        serve::<Test>(
            &mut reads,
            &writes,
            &mut commands,
            &mut id,
            Generation(4),
            &mut intents
        ),
        client,
    );
    assert_eq!(ended, Some(LossReason::Ended));
}
