use super::super::DecodeCut;
use super::*;

fn gate(harness: &Harness, samples: usize) -> mpsc::Sender<()> {
    let (release, wait) = mpsc::channel();
    assert!(
        harness
            .gates
            .lock()
            .expect("gates")
            .insert(samples, wait)
            .is_none()
    );
    release
}

#[test]
fn production_admission_refuses_live_replies_and_preserves_the_physical_slot_limit() {
    use kumochi_protocol::message::HoldFailure;
    let serial = Harness::loaded();
    let running_gate = gate(&serial, 1);
    let waiting_gate = gate(&serial, 2);
    let running = serial.decode(1).expect("running");
    assert_eq!(serial.event(), Event::Decoding(1));
    let waiting = serial.decode(2).expect("waiting");
    assert_eq!(serial.decode(3).err(), Some(SttBusy));
    serial.no_event();
    running_gate.send(()).expect("running finishes");
    assert_eq!(answer(running), text(1));
    assert_eq!(serial.event(), Event::Decoding(2));
    waiting_gate.send(()).expect("waiting finishes");
    assert_eq!(answer(waiting), text(2));

    let harness = Harness::loaded();
    let first_gate = gate(&harness, 1);
    let second_gate = gate(&harness, 2);
    let third_gate = gate(&harness, 3);
    let decode = |ask, len| {
        let (reply, receiver) = oneshot::channel();
        harness.handle.decode_for(
            ask,
            Decode {
                rate: 16_000,
                samples: vec![0; len],
                reply,
            },
        )?;
        Ok(receiver)
    };
    let first = decode(1, 1).expect("first");
    assert_eq!(harness.event(), Event::Decoding(1));
    assert_eq!(harness.handle.admit_use(), Err(HoldFailure::Busy));
    assert_eq!(decode(1, 2).err(), Some(HoldFailure::Busy));
    let second = harness.decode(2).expect("owner may queue while live");
    harness.no_event();
    drop(first);
    assert_eq!(harness.event(), Event::Decoding(2));
    drop(second);
    assert_eq!(decode(0, 3).err(), Some(HoldFailure::NotLoaded));
    assert_eq!(harness.handle.admit_use(), Ok(1));
    let third = decode(1, 3).expect("one waiter while two stale calls still occupy slots");
    assert_eq!(harness.handle.admit_use(), Err(HoldFailure::Busy));
    assert_eq!(decode(1, 4).err(), Some(HoldFailure::Busy));
    harness.no_event();
    first_gate.send(()).unwrap();
    assert_eq!(harness.event(), Event::Decoding(3));
    third_gate.send(()).unwrap();
    assert_eq!(answer(third), text(3));
    second_gate.send(()).unwrap();
}

#[test]
fn stale_decodes_allow_overlap_late_receiver_drop_and_reuse_without_exceeding_two() {
    for already_stale in [true, false] {
        let harness = Harness::loaded();
        let first_gate = gate(&harness, 1);
        let second_gate = gate(&harness, 2);
        let third_gate = gate(&harness, 3);
        let mut first = Some(harness.decode(1).expect("first"));
        assert_eq!(harness.event(), Event::Decoding(1));
        if already_stale {
            drop(first.take());
        }
        let second = harness.decode(2).expect("second");
        if !already_stale {
            harness.wait_for(|state| state.waiting.is_some(), "second was not queued");
            drop(first.take());
        }
        assert_eq!(harness.event(), Event::Decoding(2));
        let third = harness.decode(3).expect("one waiter");
        assert_eq!(harness.decode(4).err(), Some(SttBusy));
        second_gate.send(()).expect("second release");
        assert_eq!(answer(second), text(2));
        assert_eq!(harness.event(), Event::Decoding(3));
        third_gate.send(()).expect("third release");
        assert_eq!(answer(third), text(3));
        harness.handle.unload(1);
        harness.no_event();
        harness.no_report();
        first_gate.send(()).expect("first release");
        assert_eq!(harness.event(), Event::TornDown);
        assert_eq!(harness.reports(1), [report(1, LoadEvent::Unloaded)]);
    }
}

#[test]
fn an_unload_waits_for_both_calls_in_either_order_and_settles_the_waiter_immediately() {
    for first_ends_first in [None, Some(true), Some(false)] {
        let harness = Harness::loaded();
        let first_gate = gate(&harness, 1);
        let first = harness.decode(1).expect("first");
        assert_eq!(harness.event(), Event::Decoding(1));
        let (running, len, early, late) = if let Some(first_ends_first) = first_ends_first {
            drop(first);
            let second_gate = gate(&harness, 2);
            let second = harness.decode(2).expect("second");
            assert_eq!(harness.event(), Event::Decoding(2));
            let (early, late) = if first_ends_first {
                (first_gate, second_gate)
            } else {
                (second_gate, first_gate)
            };
            (second, 2, Some(early), late)
        } else {
            (first, 1, None, first_gate)
        };

        let waiter = harness.decode(3).expect("waiter");
        harness.handle.unload(1);
        assert_eq!(answer(waiter), SttResult::Stale);
        assert_eq!(
            answer(harness.decode(4).expect("refused")),
            SttResult::NotLoaded
        );
        if let Some(early) = early {
            early.send(()).expect("early release");
        }
        harness.no_event();
        harness.no_report();
        late.send(()).expect("late release");
        assert_eq!(answer(running), text(len));
        assert_eq!(harness.event(), Event::TornDown);
        assert_eq!(harness.reports(1), [report(1, LoadEvent::Unloaded)]);
        assert_eq!(
            answer(harness.decode(5).expect("refused")),
            SttResult::NotLoaded
        );
    }
}

#[test]
fn the_newest_ask_carries_the_waiter_only_after_both_old_calls_have_drained() {
    for cancel_middle in [false, true] {
        let harness = Harness::loaded();
        let first_gate = gate(&harness, 1);
        let second_gate = gate(&harness, 2);
        let third_gate = gate(&harness, 3);
        let first = harness.decode(1).expect("first");
        assert_eq!(harness.event(), Event::Decoding(1));
        drop(first);
        let second = harness.decode(2).expect("second");
        assert_eq!(harness.event(), Event::Decoding(2));
        let waiter = harness.decode(3).expect("waiter");
        harness.handle.load("superseded".into(), 2);
        let waiter = if cancel_middle {
            harness.handle.unload(2);
            assert_eq!(harness.reports(1), [report(2, LoadEvent::Unloaded)]);
            assert_eq!(answer(waiter), SttResult::Stale);
            None
        } else {
            Some(waiter)
        };
        harness.handle.load("newest".into(), 3);
        second_gate.send(()).expect("second release");
        assert_eq!(answer(second), text(2));
        harness.no_event();
        harness.no_report();
        first_gate.send(()).expect("first release");
        assert_eq!(harness.event(), Event::TornDown);
        assert_eq!(harness.event(), Event::Loaded("newest".into()));
        assert_eq!(
            harness.reports(2),
            [report(1, LoadEvent::Unloaded), report(3, LoadEvent::Loaded)]
        );
        let next = waiter.unwrap_or_else(|| harness.decode(3).expect("new owner takes fresh work"));
        assert_eq!(harness.event(), Event::Decoding(3));
        third_gate.send(()).expect("third release");
        assert_eq!(answer(next), text(3));
        harness.handle.unload(1);
        assert_eq!(harness.reports(1), [report(1, LoadEvent::Unloaded)]);
        let next = harness.decode(4).expect("new owner survived stale unload");
        assert_eq!(harness.event(), Event::Decoding(4));
        harness.release();
        assert_eq!(answer(next), text(4));
        harness.no_event();
    }
}

#[test]
fn dropping_the_owner_is_nonblocking_but_release_waits_for_both_calls() {
    let harness = Harness::loaded();
    let first_gate = gate(&harness, 1);
    let second_gate = gate(&harness, 2);
    let first = harness.decode(1).expect("first");
    assert_eq!(harness.event(), Event::Decoding(1));
    drop(first);
    let second = harness.decode(2).expect("second");
    assert_eq!(harness.event(), Event::Decoding(2));
    let waiter = harness.decode(3).expect("waiter");
    let Harness {
        handle,
        events,
        reports: _reports,
        ..
    } = harness;
    drop(handle);
    assert_eq!(answer(waiter), SttResult::Stale);
    second_gate.send(()).expect("second release");
    assert_eq!(answer(second), text(2));
    assert_eq!(events.recv_timeout(Duration::from_millis(100)).ok(), None);
    first_gate.send(()).expect("first release");
    assert_eq!(
        events.recv_timeout(WAIT).expect("teardown"),
        Event::TornDown
    );
    assert_eq!(
        events.recv_timeout(WAIT).expect("owner exit"),
        Event::Exited
    );
}

#[test]
fn idle_only_starts_after_all_running_and_queued_work_finishes() {
    use kumochi_protocol::message::SttUnload;

    for overlap in [false, true] {
        let harness = Harness::loaded();
        let first_gate = gate(&harness, 1);
        let third_gate = gate(&harness, 3);
        let first = harness.decode(1).expect("first");
        assert_eq!(harness.event(), Event::Decoding(1));
        let (active, len, active_gate, stale_gate) = if overlap {
            drop(first);
            let second_gate = gate(&harness, 2);
            let second = harness.decode(2).expect("second");
            assert_eq!(harness.event(), Event::Decoding(2));
            (second, 2, second_gate, Some(first_gate))
        } else {
            (first, 1, first_gate, None)
        };
        let third = harness.decode(3).expect("waiter");
        harness.handle.set_unload(SttUnload::After15Min);
        let old = Instant::now()
            .checked_sub(Duration::from_hours(1))
            .expect("fixture clock supports one hour of history");
        harness
            .handle
            .control
            .update(|state| state.idle_since = old);
        harness.no_event();
        harness.no_report();
        active_gate.send(()).expect("active release");
        assert_eq!(answer(active), text(len));
        assert_eq!(harness.event(), Event::Decoding(3));
        harness.no_event();
        harness.no_report();
        let before_last_release = harness.handle.control.lock().idle_since;
        third_gate.send(()).expect("third release");
        assert_eq!(answer(third), text(3));
        if let Some(stale_gate) = stale_gate {
            harness.no_event();
            harness.no_report();
            stale_gate.send(()).expect("stale release");
        }
        harness.wait_for(
            |state| state.idle_since > before_last_release,
            "no quiescent idle reset",
        );
        assert!(harness.handle.current_use(1));
        harness.no_event();
        let expired = Instant::now()
            .checked_sub(Duration::from_mins(15))
            .expect("fixture clock supports fifteen minutes of history");
        harness
            .handle
            .control
            .update(|state| state.idle_since = expired);
        assert_eq!(harness.event(), Event::TornDown);
        assert_eq!(harness.reports(1), [report(1, LoadEvent::Unloaded)]);
    }
}

#[test]
fn an_available_loss_beats_sibling_success_and_later_valid_results_cannot_revive_the_owner() {
    for loss_slot in 0..2 {
        let harness = Harness::loaded();
        let first_gate = gate(&harness, 1);
        let second_gate = gate(&harness, 2);
        let first = harness.decode(1).expect("first");
        assert_eq!(harness.event(), Event::Decoding(1));
        drop(first);
        let second = harness.decode(2).expect("second");
        assert_eq!(harness.event(), Event::Decoding(2));
        let waiter = harness.decode(3).expect("waiter");
        harness
            .handle
            .control
            .update(|state| state.decode_cut = Some(DecodeCut::Delivery));
        for (slot, finish) in [first_gate, second_gate].into_iter().enumerate() {
            harness.lose.store(slot == loss_slot, Ordering::SeqCst);
            finish.send(()).expect("release one completion");
            harness.wait_for(|state| state.finished[slot].is_some(), "completion absent");
        }
        harness
            .handle
            .control
            .update(|state| state.decode_cut = None);
        assert_eq!(answer(second), SttResult::Failed(SpeechError::WorkerLost));
        assert_eq!(answer(waiter), SttResult::NotLoaded);
        assert_eq!(harness.event(), Event::Lost);
        assert_eq!(
            answer(harness.decode(4).expect("refused")),
            SttResult::NotLoaded
        );
        harness.no_report();
        harness.release();
        assert_eq!(harness.reports(1), [report(1, LoadEvent::Lost)]);
        available_loss_fences_admission_during_join(loss_slot);
    }
}

#[test]
fn a_published_loss_fences_admission_before_owner_settlement() {
    use kumochi_protocol::message::HoldFailure;
    let harness = Harness::loaded();
    let finish = gate(&harness, 1);
    let stale = harness.decode(1).expect("first decode");
    assert_eq!(harness.event(), Event::Decoding(1));
    drop(stale);
    let resume = hold_before_serving(&harness.handle);
    harness.lose.store(true, Ordering::SeqCst);
    finish.send(()).expect("publish the loss");
    harness.wait_for(
        |state| state.finished[0] == Some(SttResult::Failed(SpeechError::WorkerLost)),
        "the fatal result was not published",
    );
    let (reply, _receive) = oneshot::channel();
    let before_settlement = (
        harness.handle.admit_use(),
        harness.handle.current_use(1),
        harness.handle.decode_for(
            1,
            Decode {
                rate: 16_000,
                samples: vec![0; 2],
                reply,
            },
        ),
    );
    resume();
    assert_eq!(harness.event(), Event::Lost);
    harness.release();
    assert_eq!(harness.reports(1), [report(1, LoadEvent::Lost)]);
    assert_eq!(
        before_settlement,
        (
            Err(HoldFailure::NotLoaded),
            false,
            Err(HoldFailure::NotLoaded)
        )
    );
}

#[test]
fn a_refused_decode_thread_is_a_service_loss_not_a_silent_dropped_reply() {
    let harness = Harness::loaded();
    harness
        .handle
        .control
        .update(|state| state.decode_cut = Some(DecodeCut::Spawn));
    assert_eq!(
        answer(harness.decode(1).expect("accepted")),
        SttResult::Failed(SpeechError::WorkerLost)
    );
    assert_eq!(harness.event(), Event::Lost);
    harness.no_report();
    harness.release();
    assert_eq!(harness.reports(1), [report(1, LoadEvent::Lost)]);
}

#[test]
fn a_current_generation_loss_fails_live_replies_but_still_drains_both_calls() {
    let harness = Harness::loaded();
    let first_gate = gate(&harness, 1);
    let second_gate = gate(&harness, 2);
    let first = harness.decode(1).expect("first");
    assert_eq!(harness.event(), Event::Decoding(1));
    drop(first);
    let second = harness.decode(2).expect("second");
    assert_eq!(harness.event(), Event::Decoding(2));
    let waiter = harness.decode(3).expect("waiter");
    harness.lose_worker();
    assert_eq!(answer(second), SttResult::Failed(SpeechError::WorkerLost));
    assert_eq!(answer(waiter), SttResult::NotLoaded);
    harness.no_event();
    harness.no_report();
    first_gate.send(()).expect("first release");
    second_gate.send(()).expect("second release");
    assert_eq!(harness.event(), Event::Lost);
    harness.no_report();
    harness.release();
    assert_eq!(harness.reports(1), [report(1, LoadEvent::Lost)]);
}

fn available_loss_fences_admission_during_join(loss_slot: usize) {
    use super::super::{Control, settle_decodes};
    use kumochi_protocol::message::HoldFailure;

    let (reports, _observed) = Reports::new();
    let (first_send, first) = oneshot::channel();
    let (second_send, second) = oneshot::channel();
    let handle = SttHandle {
        control: Arc::new(Control {
            state: Mutex::new(State {
                want: Some(("models".into(), 1)),
                current: Some(1),
                ready: true,
                waiting: None,
                replies: [Some(first_send), Some(second_send)],
                finished: std::array::from_fn(|slot| {
                    Some(if slot == loss_slot {
                        SttResult::Failed(SpeechError::WorkerLost)
                    } else {
                        text(slot + 1)
                    })
                }),
                decode_cut: None,
                hold_after_ready: false,
                unload_after: None,
                idle_since: Instant::now(),
                shutdown: false,
            }),
            wake: std::sync::Condvar::new(),
        }),
        reports,
    };
    let (_notices, mut losses) = broadcast::channel(4);
    std::thread::scope(|scope| {
        let (let_join, joined) = mpsc::channel();
        let first_task = scope.spawn(move || {
            joined
                .recv_timeout(WAIT)
                .expect("settlement reached its first join");
        });
        let second_task = scope.spawn(|| {});
        let mut running = [Some(first_task), Some(second_task)];
        let handle = &handle;
        let during_join = scope.spawn(move || {
            let until = Instant::now() + WAIT;
            while handle.control.lock().finished[0].is_some() {
                assert!(Instant::now() < until, "settlement did not reach the join");
                std::thread::yield_now();
            }
            let admission = handle.admit_use();
            let_join.send(()).expect("release join");
            admission
        });
        let mut failed = false;
        drop(settle_decodes(
            &handle.control,
            handle.control.lock(),
            &mut running,
            1,
            Generation(0),
            &mut losses,
            &mut failed,
        ));
        assert_eq!(during_join.join().unwrap(), Err(HoldFailure::NotLoaded));
        assert_eq!(answer(first), SttResult::Failed(SpeechError::WorkerLost));
        assert_eq!(answer(second), SttResult::Failed(SpeechError::WorkerLost));
    });
}

#[test]
#[cfg(panic = "unwind")]
fn a_caught_decode_unwind_is_a_service_loss_in_an_unwind_profile_only() {
    let harness = Harness::loaded();
    let release = gate(&harness, 1);
    harness.panic_at.store(1, Ordering::SeqCst);
    let reply = harness.decode(1).expect("accepted");
    assert_eq!(harness.event(), Event::Decoding(1));
    release.send(()).expect("release panic");
    assert_eq!(answer(reply), SttResult::Failed(SpeechError::WorkerLost));
    assert_eq!(harness.event(), Event::Lost);
    harness.no_report();
    harness.release();
    assert_eq!(harness.reports(1), [report(1, LoadEvent::Lost)]);
}
