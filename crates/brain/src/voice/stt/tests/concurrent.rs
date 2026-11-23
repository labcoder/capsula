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
    assert_eq!(harness.handle.admit_use().map(|(ask, _)| ask), Ok(1));
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
    for first_ends_first in [true, false] {
        let harness = Harness::loaded();
        let first_gate = gate(&harness, 1);
        let second_gate = gate(&harness, 2);
        let first = harness.decode(1).expect("first");
        assert_eq!(harness.event(), Event::Decoding(1));
        drop(first);
        let second = harness.decode(2).expect("second");
        assert_eq!(harness.event(), Event::Decoding(2));
        let waiter = harness.decode(3).expect("waiter");
        harness.handle.unload(1);
        assert_eq!(answer(waiter), SttResult::Stale);
        assert_eq!(
            answer(harness.decode(4).expect("refused")),
            SttResult::NotLoaded
        );
        let (early, late) = if first_ends_first {
            (first_gate, second_gate)
        } else {
            (second_gate, first_gate)
        };
        early.send(()).expect("early release");
        harness.no_event();
        harness.no_report();
        late.send(()).expect("late release");
        assert_eq!(answer(second), text(2));
        assert_eq!(harness.event(), Event::TornDown);
        assert_eq!(harness.reports(1), [report(1, LoadEvent::Unloaded)]);
    }
}

#[test]
fn the_newest_ask_carries_the_waiter_only_after_both_old_calls_have_drained() {
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
    harness.handle.load("newest".into(), 3);
    second_gate.send(()).expect("second release");
    assert_eq!(answer(second), text(2));
    harness.no_event();
    harness.no_report();
    first_gate.send(()).expect("first release");
    assert_eq!(harness.event(), Event::TornDown);
    assert_eq!(harness.event(), Event::Loaded("newest".into()));
    assert_eq!(harness.event(), Event::Decoding(3));
    third_gate.send(()).expect("third release");
    assert_eq!(answer(waiter), text(3));
    assert_eq!(
        harness.reports(2),
        [report(1, LoadEvent::Unloaded), report(3, LoadEvent::Loaded)]
    );
    harness.handle.unload(1);
    assert_eq!(harness.reports(1), [report(1, LoadEvent::Unloaded)]);
    assert_eq!(harness.handle.control.lock().current, Some(3));
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
    let harness = Harness::loaded();
    let first_gate = gate(&harness, 1);
    let second_gate = gate(&harness, 2);
    let third_gate = gate(&harness, 3);
    let first = harness.decode(1).expect("first");
    assert_eq!(harness.event(), Event::Decoding(1));
    drop(first);
    let second = harness.decode(2).expect("second");
    assert_eq!(harness.event(), Event::Decoding(2));
    let third = harness.decode(3).expect("waiter");
    harness.handle.set_unload(SttUnload::After15Min);
    let old = std::time::Instant::now()
        .checked_sub(Duration::from_hours(1))
        .expect("fixture clock supports one hour of history");
    harness
        .handle
        .control
        .update(|state| state.idle_since = old);
    second_gate.send(()).expect("second release");
    assert_eq!(answer(second), text(2));
    assert_eq!(harness.event(), Event::Decoding(3));
    assert_eq!(harness.handle.control.lock().idle_since, old);
    third_gate.send(()).expect("third release");
    assert_eq!(answer(third), text(3));
    assert_eq!(harness.handle.control.lock().idle_since, old);
    first_gate.send(()).expect("first release");
    harness.wait_for(|state| state.idle_since > old, "no quiescent idle reset");
    harness.no_event();
    let expired = std::time::Instant::now()
        .checked_sub(Duration::from_mins(15))
        .expect("fixture clock supports fifteen minutes of history");
    harness
        .handle
        .control
        .update(|state| state.idle_since = expired);
    harness.handle.set_unload(SttUnload::After15Min);
    assert_eq!(harness.event(), Event::TornDown);
    assert_eq!(harness.reports(1), [report(1, LoadEvent::Unloaded)]);
}

#[test]
fn an_available_loss_beats_sibling_success_and_later_valid_results_cannot_revive_the_owner() {
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
    harness.lose.store(true, Ordering::SeqCst);
    first_gate.send(()).expect("first release");
    harness.wait_for(
        |state| state.finished[0].is_some(),
        "first completion absent",
    );
    harness.lose.store(false, Ordering::SeqCst);
    second_gate.send(()).expect("second release");
    harness.wait_for(
        |state| state.finished.iter().all(Option::is_some),
        "second completion absent",
    );
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

struct LossAt(usize);

impl Decoder for LossAt {
    fn decode(&self, _: u32, samples: &[i16]) -> Result<String, SpeechError> {
        if samples.len() == self.0 {
            Err(SpeechError::WorkerLost)
        } else {
            Ok("fresh result".to_owned())
        }
    }
}

#[test]
fn either_available_slot_loss_is_observed_before_any_sibling_success() {
    use super::super::{Control, settle_decodes, spawn_decode};
    for loss in [1, 2] {
        let control = Control {
            state: Mutex::new(State {
                want: Some(("models".into(), 1)),
                current: Some(1),
                ready: true,
                waiting: None,
                replies: [None, None],
                finished: [None, None],
                decode_cut: None,
                hold_after_ready: false,
                unload_after: None,
                idle_since: std::time::Instant::now(),
                shutdown: false,
            }),
            wake: std::sync::Condvar::new(),
        };
        let (_notices, mut losses) = broadcast::channel(4);
        let decoder = LossAt(loss);
        std::thread::scope(|scope| {
            let (first_send, first) = oneshot::channel();
            let (second_send, second) = oneshot::channel();
            control.update(|state| state.replies = [Some(first_send), Some(second_send)]);
            let (let_join, joined) = mpsc::channel();
            let first_control = &control;
            let first_decoder = &decoder;
            let first_task = scope.spawn(move || {
                let result = match first_decoder.decode(16_000, &[0]) {
                    Ok(text) => SttResult::Text(text),
                    Err(error) => SttResult::Failed(error),
                };
                first_control.update(|state| state.finished[0] = Some(result));
                joined
                    .recv_timeout(WAIT)
                    .expect("settlement reached its first join");
            });
            let second_task = spawn_decode(scope, &control, &decoder, 1, 16_000, vec![0; 2], None)
                .expect("second thread");
            let mut running = [Some(first_task), Some(second_task)];
            let mut state = control.lock();
            let until = std::time::Instant::now() + WAIT;
            while state.finished.iter().any(Option::is_none) {
                let left = until
                    .checked_duration_since(std::time::Instant::now())
                    .expect("both completions publish");
                state = control
                    .wake
                    .wait_timeout(state, left)
                    .expect("completion wake")
                    .0;
            }
            let control = &control;
            let during_join = scope.spawn(move || {
                let until = Instant::now() + WAIT;
                loop {
                    let state = control.lock();
                    if state.finished[0].is_none() {
                        let ready = state.ready;
                        let_join.send(()).unwrap();
                        return ready;
                    }
                    drop(state);
                    assert!(
                        Instant::now() < until,
                        "settlement never reached its first join"
                    );
                    std::thread::yield_now();
                }
            });
            let mut failed = false;
            drop(settle_decodes(
                control,
                state,
                &mut running,
                1,
                Generation(0),
                &mut losses,
                &mut failed,
            ));
            assert!(
                !during_join.join().unwrap(),
                "known worker loss must fence admission before join"
            );
            assert!(failed);
            assert!(running.iter().all(Option::is_none));
            assert!(!control.lock().ready);
            assert_eq!(answer(first), SttResult::Failed(SpeechError::WorkerLost));
            assert_eq!(answer(second), SttResult::Failed(SpeechError::WorkerLost));
        });
    }
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
