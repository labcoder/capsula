use super::*;

#[derive(Clone, Copy, Debug)]
enum Delivery {
    CancelBefore,
    ExpireBefore,
    CancelAfterSend,
    AtDeadline,
    AfterDeadline,
    Timely,
}

#[tokio::test(start_paused = true)]
async fn selected_resource_destinations_keep_their_original_deadline_and_guard_through_send() {
    for expected in [None, Some(Generation(0))] {
        for schedule in [
            Delivery::CancelBefore,
            Delivery::ExpireBefore,
            Delivery::CancelAfterSend,
            Delivery::AtDeadline,
            Delivery::AfterDeadline,
            Delivery::Timely,
        ] {
            let (handle, mut commands, intents) = handle();
            let until = Instant::now() + Duration::from_secs(1);
            let mut request = Box::pin(handle.request_resource(vec![7], until, expected, None));
            assert!(
                request
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            let mut selected = commands.try_recv().expect("admitted destination").answer;
            selected.dispatched(Generation(0));
            if matches!(schedule, Delivery::ExpireBefore) {
                tokio::time::advance(Duration::from_secs(1)).await;
            }
            if matches!(schedule, Delivery::CancelBefore) {
                drop(request);
                assert_eq!(selected.deliver("terminal".into(), Generation(0)), Ok(true));
            } else {
                assert_eq!(selected.deliver("terminal".into(), Generation(0)), Ok(true));
                if matches!(schedule, Delivery::CancelAfterSend) {
                    drop(request);
                } else {
                    if matches!(schedule, Delivery::AtDeadline | Delivery::AfterDeadline) {
                        tokio::time::advance(Duration::from_secs(1)).await;
                    }
                    if matches!(schedule, Delivery::AfterDeadline) {
                        tokio::time::advance(Duration::from_nanos(1)).await;
                    }
                    let result = match request.await {
                        Ok(completion) => completion.handoff(),
                        Err(error) => Err(error),
                    };
                    if matches!(schedule, Delivery::Timely) {
                        assert_eq!(result, Ok(("terminal".into(), Generation(0))));
                    } else {
                        assert_eq!(result, Err(WorkerLost::Lost), "{schedule:?}");
                    }
                }
            }
            assert_eq!(
                *intents.borrow(),
                if matches!(schedule, Delivery::Timely) {
                    None
                } else {
                    Some(Generation(0))
                },
                "{schedule:?}, {expected:?}"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn timeout_before_selected_delivery_destroys_the_receiver_in_both_resource_modes() {
    for expected in [None, Some(Generation(0))] {
        let (handle, mut commands, intents) = handle();
        let until = Instant::now() + Duration::from_secs(1);
        let mut request = Box::pin(handle.request_resource(vec![7], until, expected, None));
        assert!(
            request
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let mut selected = commands.try_recv().expect("admitted destination").answer;
        selected.dispatched(Generation(0));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(matches!(request.await, Err(WorkerLost::Lost)));
        assert!(
            selected.is_closed(),
            "timeout destroyed the receiver before delivery"
        );
        assert_eq!(*intents.borrow(), None);
        assert_eq!(selected.deliver("terminal".into(), Generation(0)), Ok(true));
        assert_eq!(*intents.borrow(), Some(Generation(0)));
    }
}

#[tokio::test]
async fn owned_guard_precedes_admission_and_never_changes_to_a_reply_generation() {
    let (handle, mut commands, intents) = handle();
    drop(handle.request_resource(vec![1], Instant::now() + WINDOW, Some(Generation(0)), None));
    assert_eq!(*intents.borrow(), Some(Generation(0)));
    let mut request = Box::pin(handle.request_resource(
        vec![2],
        Instant::now() + WINDOW,
        Some(Generation(0)),
        None,
    ));
    assert!(
        request
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let mut command = commands.try_recv().expect("owned command");
    command.answer.dispatched(Generation(0));
    assert_eq!(
        command.answer.deliver("unloaded".into(), Generation(1)),
        Ok(true)
    );
    assert_eq!(
        request.await.expect("terminal envelope").handoff(),
        Err(WorkerLost::Lost)
    );
    assert_eq!(*intents.borrow(), Some(Generation(0)));
}

#[tokio::test]
async fn only_an_owned_resource_has_an_obligation_when_admission_is_refused() {
    for fatal in [false, true] {
        let (handle, _commands, intents) = handle();
        for _ in 0..QUEUE {
            let mut request = Box::pin(handle.request(vec![1], Instant::now() + WINDOW));
            assert!(
                request
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
        }
        handle.notices.fatal.store(fatal, Ordering::Release);
        let error = if fatal {
            WorkerLost::ExitUnconfirmed
        } else {
            WorkerLost::Lost
        };
        assert!(
            matches!(handle.request_resource(vec![2], Instant::now() + WINDOW, None, None).await, Err(got) if got == error)
        );
        assert_eq!(*intents.borrow(), None);
        assert!(
            matches!(handle.request_resource(vec![3], Instant::now() + WINDOW, Some(Generation(0)), None).await, Err(got) if got == error)
        );
        assert_eq!(*intents.borrow(), Some(Generation(0)));
    }
}

#[tokio::test]
async fn ordinary_unconsumed_replies_do_not_request_retirement() {
    let (handle, mut commands, intents) = handle();
    let mut request = Box::pin(handle.request(vec![1], Instant::now() + WINDOW));
    assert!(
        request
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let mut command = commands.try_recv().expect("ordinary command");
    assert_eq!(
        command.answer.deliver("ordinary".into(), Generation(0)),
        Ok(true)
    );
    drop(request);
    assert_eq!(*intents.borrow(), None);
}

#[test]
fn synchronized_max_is_instance_local_including_zero_and_duplicate_notifications() {
    for order in [
        [Generation(3), Generation(8)],
        [Generation(8), Generation(3)],
    ] {
        let (retirement, mut intents) = resource::Retirement::channel();
        let (other, other_intents) = resource::Retirement::channel();
        assert_eq!(*intents.borrow(), None);
        assert_eq!(retirement.request(Generation(0)), Ok(()));
        assert_eq!(*intents.borrow_and_update(), Some(Generation(0)));
        assert_eq!(other.request(Generation(0)), Ok(()));
        for generation in order {
            assert_eq!(retirement.request(generation), Ok(()));
        }
        let producers: Vec<_> = (0..16)
            .map(|generation| {
                let retirement = retirement.clone();
                std::thread::spawn(move || retirement.request(Generation(generation)))
            })
            .collect();
        for producer in producers {
            assert_eq!(producer.join().expect("producer joins"), Ok(()));
        }
        assert_eq!(*intents.borrow_and_update(), Some(Generation(15)));
        assert_eq!(retirement.request(Generation(0)), Ok(()));
        assert_eq!(retirement.request(Generation(15)), Ok(()));
        assert!(!intents.has_changed().expect("open lane"));
        assert_eq!(*other_intents.borrow(), Some(Generation(0)));
        drop(intents);
        assert_eq!(
            retirement.request(Generation(15)),
            Err(WorkerLost::ExitUnconfirmed)
        );
    }
}

#[tokio::test]
async fn an_old_owned_unload_neither_writes_nor_retires_the_replacement() {
    let (handle, mut commands, mut intents) = handle();
    let (writes, written) = blocking::sync_channel(QUEUE);
    let (frames, mut reads) = mpsc::channel(QUEUE);
    let mut id = 0;
    let mut serving = Box::pin(serve::<Test>(
        &mut reads,
        &writes,
        &mut commands,
        &mut id,
        Generation(1),
        &mut intents,
    ));
    let mut request = Box::pin(handle.request_resource(
        vec![9],
        Instant::now() + WINDOW,
        Some(Generation(0)),
        None,
    ));
    let mut context = Context::from_waker(Waker::noop());
    assert!(request.as_mut().poll(&mut context).is_pending());
    assert!(serving.as_mut().poll(&mut context).is_pending());
    assert!(matches!(request.await, Err(WorkerLost::Lost)));
    assert!(matches!(
        written.try_recv(),
        Err(blocking::TryRecvError::Empty)
    ));
    assert!(serving.as_mut().poll(&mut context).is_pending());
    frames.send(None).await.expect("end replacement");
    assert_eq!(serving.await, Some(LossReason::Ended));
}

#[tokio::test]
async fn generation_exhaustion_confirms_the_current_worker_without_attempting_a_replacement() {
    let mut rig = fixture::start_at::<Test>(Generation(u64::MAX));
    let mut exits = rig.handle.exits();
    assert_eq!(
        rig.handle.retirement().request(Generation(u64::MAX)),
        Ok(())
    );
    rig.held().await;
    assert_eq!(exits.try_recv(), Err(broadcast::error::TryRecvError::Empty));
    rig.confirm.send(true).expect("physical confirmation");
    tokio::time::timeout(Duration::from_secs(5), rig.actor)
        .await
        .expect("exhaustion stops")
        .expect("actor stops at exhaustion");
    assert_eq!(rig.spawns.load(Ordering::Relaxed), 0);
    assert_eq!(
        exits.try_recv(),
        Ok(Exit {
            generation: Generation(u64::MAX),
            confirmed: true
        })
    );
    assert_eq!(
        rig.handle.retirement().request(Generation(u64::MAX)),
        Err(WorkerLost::ExitUnconfirmed)
    );
    assert_eq!(
        rig.handle.request(vec![1], Instant::now() + WINDOW).await,
        Err(WorkerLost::Lost)
    );
}

#[tokio::test]
async fn two_real_supervisors_at_generation_zero_do_not_share_retirement() {
    let mut first = fixture::start::<Test>();
    let mut second = fixture::start::<Test>();
    assert_eq!(first.handle.retirement().request(Generation(0)), Ok(()));
    first.held().await;
    assert_eq!(second.terminations.load(Ordering::Relaxed), 0);
    let sender = second.handle.clone();
    let request = sender.request(vec![8], Instant::now() + WINDOW);
    let answer = async {
        let (id, body) = second.next().await;
        assert_eq!(body, [8]);
        second.reply(id, &"healthy".into()).await;
    };
    let (reply, ()) = tokio::join!(request, answer);
    assert_eq!(reply, Ok(("healthy".into(), Generation(0))));
    assert_eq!(second.terminations.load(Ordering::Relaxed), 0);
    drop(first.handle);
    first.confirm.send(true).expect("first confirms");
    first.actor.await.expect("first ends");
    drop(sender);
    drop(second.handle);
    second.confirm.send(true).expect("shutdown confirms");
    second.actor.await.expect("second ends");
    assert_eq!(second.terminations.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn stale_or_sender_closed_retirement_does_not_spin_or_starve_ordinary_work() {
    for close in [false, true] {
        let (handle, mut commands, mut intents) = handle();
        let retirement = handle.retirement;
        assert_eq!(retirement.request(Generation(0)), Ok(()));
        let sender = handle.commands;
        if close {
            drop(retirement);
        }
        let (writes, written) = blocking::sync_channel(QUEUE);
        let (frames, mut reads) = mpsc::channel(QUEUE);
        let mut id = 0;
        let mut serving = Box::pin(serve::<Test>(
            &mut reads,
            &writes,
            &mut commands,
            &mut id,
            Generation(1),
            &mut intents,
        ));
        let mut context = Context::from_waker(Waker::noop());
        for expected_id in 1..=2 {
            let (answer, mut reply) = oneshot::channel();
            sender
                .try_send(Command {
                    body: vec![1],
                    until: Instant::now() + WINDOW,
                    answer: Destination::Single(answer),
                })
                .unwrap_or_else(|_| panic!("ordinary command admitted"));
            assert!(serving.as_mut().poll(&mut context).is_pending());
            assert_eq!(
                written.try_recv().expect("ordinary work cannot starve"),
                (expected_id, vec![1])
            );
            for frame in self::frames(expected_id, &"reply", Test::MAX_REPLY) {
                frames.try_send(Some(frame)).expect("reply queue");
            }
            assert!(serving.as_mut().poll(&mut context).is_pending());
            assert_eq!(reply.try_recv(), Ok(Ok(("reply".into(), Generation(1)))));
        }
        frames.send(None).await.expect("worker ends");
        assert_eq!(serving.await, Some(LossReason::Ended));
    }
}

#[tokio::test]
async fn retained_matching_intent_precedes_ready_reply_and_command_floods() {
    let (handle, mut commands, mut intents) = handle();
    let (writes, written) = blocking::sync_channel(QUEUE);
    let (frames, mut reads) = mpsc::channel(QUEUE);
    let mut id = 0;
    let mut request = Box::pin(handle.request(vec![1], Instant::now() + WINDOW));
    assert!(
        request
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    for frame in self::frames(99, &"ready", Test::MAX_REPLY) {
        frames.try_send(Some(frame)).expect("ready frame");
    }
    assert_eq!(handle.retirement().request(Generation(0)), Ok(()));
    assert_eq!(
        serve::<Test>(
            &mut reads,
            &writes,
            &mut commands,
            &mut id,
            Generation(0),
            &mut intents
        )
        .await,
        Some(LossReason::Retired)
    );
    assert!(matches!(
        written.try_recv(),
        Err(blocking::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn an_old_intent_during_confirmation_does_not_retire_the_replacement() {
    let rig = confirming().await;
    assert_eq!(rig.handle.retirement().request(Generation(0)), Ok(()));
    assert_eq!(rig.spawns.load(Ordering::Relaxed), 0);
    rig.decide.send(true).expect("old physical exit");
    let (requests, replies) = rig.next;
    let (id, requests) = fixture::receive_request(requests).await;
    assert_eq!(id.1, b"held");
    for frame in self::frames(id.0, &"replacement", Test::MAX_REPLY) {
        replies
            .send(Some(frame))
            .await
            .expect("replacement replies");
    }
    assert_eq!(rig.held.await, Ok(("replacement".into(), Generation(1))));
    assert_eq!(rig.handle.retirement().request(Generation(0)), Ok(()));
    let request = rig.handle.request(vec![7], Instant::now() + WINDOW);
    let answer = async {
        let ((id, body), _requests) = fixture::receive_request(requests).await;
        assert_eq!(body, [7]);
        for frame in self::frames(id, &"still healthy", Test::MAX_REPLY) {
            replies
                .send(Some(frame))
                .await
                .expect("replacement replies");
        }
    };
    let (result, ()) = tokio::join!(request, answer);
    assert_eq!(result, Ok(("still healthy".into(), Generation(1))));
    assert_eq!(rig.spawns.load(Ordering::Relaxed), 1);
    drop(rig.handle);
    rig.running.await.expect("actor joins");
}

#[tokio::test]
async fn completion_debug_never_formats_the_reply_content() {
    let (handle, mut commands, _intents) = handle();
    let mut request =
        Box::pin(handle.request_resource(vec![], Instant::now() + WINDOW, None, None));
    assert!(
        request
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let mut destination = commands.try_recv().expect("resource command").answer;
    destination.dispatched(Generation(0));
    let marker = "resource-private-reply-sentinel";
    assert_eq!(destination.deliver(marker.into(), Generation(0)), Ok(true));
    let completion = request.await.expect("guarded reply");
    let debug = format!("{completion:?}");
    assert!(!debug.contains(marker));
    assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
    assert_eq!(completion.handoff(), Ok((marker.into(), Generation(0))));
}

#[tokio::test]
async fn a_retained_completion_does_not_keep_commands_alive_or_skip_final_exit_confirmation() {
    let (decide, exited) = blocking::channel();
    let (asked, asks) = blocking::channel();
    let terminations = Arc::new(AtomicUsize::new(0));
    let (worker, (written, replies)) = staged(Some(Box::new(Counted {
        held: Held { asked, exited },
        terminations: Arc::clone(&terminations),
    })));
    let (handle, commands, intents) = handle();
    let notices = handle.notices.clone();
    let mut exits = handle.exits();
    let mut actor = Box::pin(run(
        || Err(SpawnFailure::Failed(io::Error::other("no replacement"))),
        worker,
        commands,
        notices,
        intents,
        Generation(0),
    ));
    let mut request =
        Box::pin(handle.request_resource(vec![1], Instant::now() + WINDOW, None, None));
    let mut context = Context::from_waker(Waker::noop());
    assert!(request.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let (id, body) = written.try_recv().expect("actual dispatch");
    assert_eq!(body, [1]);
    for frame in self::frames(id, &"held completion", Test::MAX_REPLY) {
        replies.try_send(Some(frame)).expect("actual reply");
    }
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let completion = request.await.expect("the reply guard stays armed");
    assert_eq!(terminations.load(Ordering::Relaxed), 0);
    drop(handle);
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let (wait, _asks) = fixture::receive_request(asks).await;
    assert_eq!(wait, EXIT_CONFIRMATION);
    assert_eq!(terminations.load(Ordering::Relaxed), 1);
    assert_eq!(exits.try_recv(), Err(broadcast::error::TryRecvError::Empty));
    decide.send(true).expect("final physical exit");
    actor.await;
    assert_eq!(
        exits.try_recv(),
        Ok(Exit {
            generation: Generation(0),
            confirmed: true
        })
    );
    assert_eq!(
        exits.try_recv(),
        Err(broadcast::error::TryRecvError::Closed)
    );
    drop(completion);
    assert_eq!(terminations.load(Ordering::Relaxed), 1);
}

struct Counted {
    held: Held,
    terminations: Arc<AtomicUsize>,
}

impl Process for Counted {
    fn terminate(&self) -> io::Result<()> {
        self.terminations.fetch_add(1, Ordering::Relaxed);
        self.held.terminate()
    }

    fn exits_within(&self, timeout: Duration) -> io::Result<bool> {
        self.held.exits_within(timeout)
    }
}

#[tokio::test]
async fn dropping_a_successfully_sent_unconsumed_resource_causes_retirement_without_another_rpc() {
    let (decide, exited) = blocking::channel();
    let (asked, _asks) = blocking::channel();
    let terminations = Arc::new(AtomicUsize::new(0));
    let (worker, (written, replies)) = staged(Some(Box::new(Counted {
        held: Held { asked, exited },
        terminations: Arc::clone(&terminations),
    })));
    let (handle, commands, intents) = handle();
    let notices = handle.notices.clone();
    let mut exits = handle.exits();
    let mut actor = Box::pin(run(
        || {
            Err(SpawnFailure::Failed(io::Error::other(
                "no replacement in this fixture",
            )))
        },
        worker,
        commands,
        notices.clone(),
        intents,
        Generation(0),
    ));
    let (track, receipt) = watch::channel(Dispatch::Pending);
    let mut request = Box::pin(handle.request_tracked(vec![3], Instant::now() + WINDOW, track));
    let mut context = Context::from_waker(Waker::noop());
    assert_eq!(request.as_mut().poll(&mut context), Poll::Pending);
    assert_eq!(actor.as_mut().poll(&mut context), Poll::Pending);
    assert_eq!(*receipt.borrow(), Dispatch::Dispatched(Generation(0)));
    let (id, body) = written.try_recv().expect("the real actor dispatches");
    assert_eq!(body, [3]);
    for frame in frames(id, &"loaded", Test::MAX_REPLY) {
        replies.try_send(Some(frame)).expect("reply queue");
    }
    assert_eq!(actor.as_mut().poll(&mut context), Poll::Pending);
    assert_eq!(terminations.load(Ordering::Relaxed), 0);
    drop(request);
    assert_eq!(actor.as_mut().poll(&mut context), Poll::Pending);
    assert_eq!(terminations.load(Ordering::Relaxed), 1);
    assert_eq!(exits.try_recv(), Err(broadcast::error::TryRecvError::Empty));
    drop(handle);
    decide.send(true).expect("confirm the caused retirement");
    actor.await;
    assert_eq!(terminations.load(Ordering::Relaxed), 1);
    assert_eq!(
        exits.try_recv(),
        Ok(Exit {
            generation: Generation(0),
            confirmed: true,
        })
    );
    assert_eq!(exits.try_recv(), Err(broadcast::error::TryRecvError::Empty));
}
