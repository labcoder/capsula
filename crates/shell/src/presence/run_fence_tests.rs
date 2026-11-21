//! A hold recognized over a spoken run stops that run, the brain's `Stop` first and the
//! speaker's only once it is queued; Esc keeps the same order. The speaker is scripted.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use gpui_kit::{Entity, TestAppContext};
use kumochi_protocol::message::{ControlToBrain, GestureId, Phase, RunId, ThreadId};

use super::Presence;
use super::voice::Stamp;
use super::voice_wiring_tests::{gate, hold, key_down, ready};
use crate::input::TalkHeld;
use crate::input::hold::Intent;
use crate::strings::Text;
use kumochi_shell_core::speaker::{Command, Snapshot, Tail};

/// Run 5's content committed, with nothing played yet.
const SPOKEN: Snapshot = Snapshot {
    exposed: 480,
    played: 0,
    flight: 0,
    quiet: 0,
    flags: 0,
    spoken: 5,
};

/// Run 5 answers on a live output whose tail reads `now`. The tail is kept, as a speaker's
/// worker would keep it.
fn answering(
    owner: &Entity<Presence>,
    now: Snapshot,
    cx: &mut TestAppContext,
) -> (Receiver<Command>, Arc<Tail>) {
    owner.update(cx, |owner, _| {
        owner
            .runs
            .apply_state(RunId(5), ThreadId(1), Phase::Answering, None);
        assert_eq!(owner.runs.stoppable(), Some(RunId(5)), "run 5 moves");
        let (commands, tail) = owner.link.output().scripted(1);
        tail.publish(&now);
        (commands, tail)
    })
}

fn warm(owner: &Entity<Presence>, gesture: u32, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        key_down(owner, gesture, cx);
        let stamp = Stamp {
            target: owner.voice.target.get(),
            session: 1,
            choice: owner.link.choice_revision(),
        };
        owner.voice_intent(
            Intent::Warm(GestureId(gesture)),
            stamp,
            0,
            None,
            TalkHeld::default(),
            cx,
        );
    });
}

/// Brain and speaker stops in the order they were queued: the witness drains the brain's
/// queue each time the speaker takes a command, and `order` drains what came after.
type Order = Arc<Mutex<Vec<(&'static str, RunId)>>>;

fn witness(
    owner: &Entity<Presence>,
    controls: &async_channel::Receiver<ControlToBrain>,
    cx: &mut TestAppContext,
) -> Order {
    let seen = Order::default();
    let (log, brain) = (Arc::clone(&seen), controls.clone());
    owner.update(cx, |owner, _| {
        owner.link.output().witness(Box::new(move |command| {
            let mut log = log.lock().expect("unpoisoned");
            log.extend(brain_stops(&brain).into_iter().map(|run| ("brain", run)));
            if let Command::Stop(run) = command {
                log.push(("speaker", *run));
            }
        }));
    });
    seen
}

fn order(
    seen: &Order,
    controls: &async_channel::Receiver<ControlToBrain>,
) -> Vec<(&'static str, RunId)> {
    let mut log = seen.lock().expect("unpoisoned");
    log.extend(brain_stops(controls).into_iter().map(|run| ("brain", run)));
    std::mem::take(&mut *log)
}

fn stops(commands: &Receiver<Command>) -> Vec<RunId> {
    commands
        .try_iter()
        .filter_map(|command| match command {
            Command::Stop(run) => Some(run),
            _ => None,
        })
        .collect()
}

fn brain_stops(controls: &async_channel::Receiver<ControlToBrain>) -> Vec<RunId> {
    std::iter::from_fn(|| controls.try_recv().ok())
        .filter_map(|message| match message {
            ControlToBrain::Stop { run, .. } | ControlToBrain::StopScenes { run, .. } => Some(run),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn recognition_stops_the_run_spoken_at_key_down_once(cx: &mut TestAppContext) {
    let (owner, _data, controls) = ready(cx);
    let (_commands, _tail) = answering(&owner, SPOKEN, cx);
    let seen = witness(&owner, &controls, cx);
    warm(&owner, 1, cx);
    owner.update(cx, |owner, _| {
        assert_eq!(
            owner.voice.gates.run(GestureId(1)),
            Some((1, RunId(5))),
            "captured"
        );
    });
    hold(&owner, 1, cx);
    assert_eq!(
        order(&seen, &controls),
        [("brain", RunId(5)), ("speaker", RunId(5))],
        "the brain's Stop, then the speaker's"
    );
}

#[gpui::test]
fn a_refused_brain_stop_never_stops_the_speaker_and_opens_no_hold(cx: &mut TestAppContext) {
    let (owner, _data, controls) = ready(cx);
    let (commands, _tail) = answering(&owner, SPOKEN, cx);
    warm(&owner, 1, cx);
    drop(controls);
    owner.update(cx, |owner, cx| {
        let gesture = GestureId(1);
        gate(owner, 1, cx);
        let token = owner.link.microphone_gate().arm(gesture).expect("fresh");
        owner.link.track_input(gesture);
        let stamp = Stamp {
            target: owner.voice.target.get(),
            session: 1,
            choice: owner.link.choice_revision(),
        };
        owner.voice_intent(
            Intent::Recognized {
                gesture,
                generation: Ok(token),
            },
            stamp,
            0,
            None,
            TalkHeld::default(),
            cx,
        );
        assert!(owner.voice.pending.is_none(), "no hold");
        assert_eq!(owner.voice.status, Some(Text::HoldFailed));
    });
    assert_eq!(stops(&commands), [], "the speaker wasn't stopped");
}

#[gpui::test]
fn a_run_with_no_spoken_content_gets_no_recognition_stop(cx: &mut TestAppContext) {
    let (owner, _data, controls) = ready(cx);
    let (commands, _tail) = answering(
        &owner,
        Snapshot {
            spoken: 0,
            ..SPOKEN
        },
        cx,
    );
    warm(&owner, 1, cx);
    hold(&owner, 1, cx);
    assert_eq!(brain_stops(&controls), [], "the moving run keeps going");
    assert_eq!(stops(&commands), []);
}

#[gpui::test]
fn a_run_replaced_after_key_down_is_not_stopped(cx: &mut TestAppContext) {
    let (owner, _data, controls) = ready(cx);
    let (commands, _tail) = answering(&owner, SPOKEN, cx);
    warm(&owner, 1, cx);
    owner.update(cx, |owner, _| {
        owner
            .runs
            .apply_state(RunId(6), ThreadId(1), Phase::Answering, None);
    });
    hold(&owner, 1, cx);
    assert_eq!(brain_stops(&controls), []);
    assert_eq!(stops(&commands), []);
}

#[gpui::test]
fn escape_stops_the_speaker_after_the_brain_and_never_without_it(cx: &mut TestAppContext) {
    let (owner, _data, controls) = ready(cx);
    let (_commands, _tail) = answering(&owner, SPOKEN, cx);
    let seen = witness(&owner, &controls, cx);
    escape(&owner, cx);
    assert_eq!(
        order(&seen, &controls),
        [("brain", RunId(5)), ("speaker", RunId(5))]
    );

    let (owner, _data, controls) = ready(cx);
    let (commands, _tail) = answering(&owner, SPOKEN, cx);
    drop(controls);
    escape(&owner, cx);
    assert_eq!(stops(&commands), [], "the brain wasn't told");
}

#[cfg(all(windows, debug_assertions))]
#[test]
fn legacy_authenticated_pair_still_admits_and_stops_a_typed_run() {
    use crate::link::{LinkEvent, fixture};
    use kumochi_protocol::message::{AudioSession, DataToBrain, RunOrigin, ScreenChoice};

    let (id, hello, brain) = kumochi_brain::ipc::offline_audio_peer("5000 text Late.".into());
    let pipe = fixture::authenticated(id, hello, AudioSession::Legacy);
    let events = fixture::patient(pipe.events);
    let session = fixture::connected(fixture::next(&events));
    let thread = loop {
        if let LinkEvent::Settings { view, .. } = fixture::next(&events) {
            break view.main_thread;
        }
    };
    assert!(pipe.handle.send_data(
        &session,
        DataToBrain::TextSubmit {
            turn: 1,
            thread: Some(thread),
            screen: ScreenChoice::None,
            text: "legacy turn".into(),
        }
    ));
    let run = loop {
        if let LinkEvent::RunState {
            run, phase, origin, ..
        } = fixture::next(&events)
        {
            assert_eq!(
                (phase, origin),
                (Phase::Thinking, Some(RunOrigin::Typed { turn: 1 }))
            );
            break run;
        }
    };
    assert!(pipe.handle.send_control(
        &session,
        ControlToBrain::Stop {
            run,
            revealed: Vec::new()
        }
    ));
    loop {
        if let LinkEvent::RunState {
            run: current,
            phase,
            ..
        } = fixture::next(&events)
        {
            assert_eq!((current, phase), (run, Phase::Interrupted));
            break;
        }
    }
    pipe.handle.end(&session);
    assert!(pipe.link.join().expect("Shell link joined"));
    assert!(brain.join().expect("Brain joined"));
}

#[cfg(all(windows, debug_assertions))]
fn submit_draft(owner: &Entity<Presence>, draft: &'static str, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        let text = owner.text.as_ref().unwrap();
        text.window
            .update(cx, |_, window, cx| {
                text.input
                    .update(cx, |input, cx| input.set_value(draft, window, cx));
            })
            .unwrap();
        owner.submit(cx).expect("one fresh composer action");
    });
}

#[cfg(all(windows, debug_assertions))]
#[gpui::test]
#[expect(
    clippy::too_many_lines,
    reason = "one authenticated session from early intent through active playback and Esc"
)]
fn queued_plain_escape_stops_active_audio_while_brain_data_credit_is_exhausted(
    cx: &mut TestAppContext,
) {
    use crate::link::{Batch, LinkEvent, fixture};
    use kumochi_protocol::message::DataToShell;
    use std::time::{Duration, Instant};

    const WAIT: Duration = Duration::from_secs(3);
    fn wait(label: &str, mut ready: impl FnMut() -> bool) {
        let limit = Instant::now() + WAIT;
        while !ready() {
            assert!(
                Instant::now() < limit,
                "{label} before the fixture deadline"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let _device = fixture::serialize_playback();
    let script = "0 text Distinct sound. More\n0 step kept\n1800 step too late\n5000 text Never.";
    let (id, hello, probe, brain) = kumochi_brain::ipc::offline_plain_peer(script.into(), 2_200);
    let pipe = fixture::authenticated_primed(id, hello);
    let events = fixture::patient(pipe.events);
    let owner = super::test_fixture::presence(cx);
    owner.update(cx, |owner, _| owner.link = pipe.handle.clone());
    let apply = |event, cx: &mut TestAppContext| {
        owner.update(cx, |owner, cx| {
            owner.link_events(Batch::Plain(vec![event]), cx).unwrap();
        });
    };
    let session = fixture::connected(fixture::next(&events));
    apply(LinkEvent::Connected(session.clone()), cx);
    loop {
        let event = fixture::next(&events);
        let settings = matches!(&event, LinkEvent::Settings { .. });
        apply(event, cx);
        if settings {
            break;
        }
    }
    assert!(
        !pipe.handle.admits_text(Some(&session)),
        "the actual ACK is still withheld"
    );
    submit_draft(&owner, "before ACK", cx);
    owner.read_with(cx, |owner, cx| {
        assert_eq!(
            owner.text.as_ref().unwrap().input.read(cx).value().as_ref(),
            "before ACK"
        );
    });
    probe.release_ack();
    wait("exact ACK", || pipe.handle.admits_text(Some(&session)));
    std::thread::sleep(Duration::from_millis(5_100));
    assert!(
        session.open(),
        "an idle data read outlives the handshake timeout"
    );
    submit_draft(&owner, "after ACK", cx);

    let run = loop {
        let event = fixture::next(&events);
        assert!(!matches!(event, LinkEvent::RunState { .. }));
        let early = match &event {
            LinkEvent::Data {
                message: DataToShell::AssistantText { run, .. },
                ..
            } => Some(*run),
            _ => None,
        };
        apply(event, cx);
        if let Some(run) = early {
            break run;
        }
    };
    assert!(matches!(
        pipe.playback.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    probe.release_run();
    let (mut answering, mut drawn) = (false, false);
    while !answering || !drawn {
        let event = fixture::next(&events);
        answering |= matches!(&event, LinkEvent::RunState { run: current, phase: Phase::Answering, .. } if *current == run);
        drawn |= matches!(&event, LinkEvent::Data { message: DataToShell::StepCommit { run: current, .. }, .. } if *current == run);
        apply(event, cx);
    }
    owner.read_with(cx, |owner, _| assert!(owner.runs.drawing()));
    let mut playback = pipe
        .playback
        .recv_timeout(WAIT)
        .expect("the real worker opened its fake device");
    let mut samples = [0.0; 48];
    wait("worker stages real PCM", || playback.ready());
    playback.render(&mut samples);
    assert_eq!(
        samples.map(f32::to_bits),
        [(15_430.0_f32 / 32_768.0).to_bits(); 48]
    );
    assert_eq!(
        playback.handed(),
        48,
        "the first callback starts at source sample zero"
    );
    wait("Brain data queue exhausts its credit", || {
        if playback.ready() {
            playback.render(&mut samples);
        }
        probe.data_blocked()
    });
    assert!(!playback.stopped());
    owner.read_with(cx, |owner, _| assert_eq!(owner.runs.stoppable(), Some(run)));
    assert!(!pipe.handle.admits_text(Some(&session)));
    wait("returned Shell credit reaches Brain", || {
        probe.data_credits().0 > 0
    });
    assert_eq!(probe.data_credits().1, 0);
    wait(
        "source remains after Brain data credit is exhausted",
        || playback.ready(),
    );
    fixture::with_outbox_held(&pipe.handle, &session, || {
        playback.render(&mut samples);
        assert_eq!(
            samples.map(f32::to_bits),
            [(15_430.0_f32 / 32_768.0).to_bits(); 48],
            "the callback still renders the identifiable source immediately before Esc"
        );
        escape(&owner, cx);
        owner.read_with(cx, |owner, _| {
            assert_eq!(
                owner.runs.stoppable(),
                None,
                "Esc fenced the drawn run while the exact outbox was still locked"
            );
        });
    });
    wait("local fence and Brain Stop on control", || {
        playback.stopped() && probe.stop() == run
    });
    assert_eq!(
        probe.data_credits().1,
        0,
        "Brain took Stop before applying credit"
    );
    assert_eq!(
        probe.quiet(),
        RunId(0),
        "the physical tail has not been confirmed"
    );
    assert!(
        !pipe.handle.admits_text(Some(&session)),
        "the unconfirmed tail still owns output"
    );
    let mut after = [99.0; 128];
    playback.render(&mut after);
    assert!(after[119..].iter().all(|sample| sample.to_bits() == 0));
    playback.render(&mut after);
    assert_eq!(after.map(f32::to_bits), [0; 128]);
    wait("Brain accepts the original run's quiet receipt", || {
        probe
            .accepted_quiet()
            .is_some_and(|(stopped, _)| stopped == run)
    });
    let cursor = assert_heard_prefix(&probe, run);
    assert_eq!(probe.accepted_quiet(), Some((run, Some(cursor))));
    loop {
        let event = fixture::next(&events);
        let interrupted = matches!(&event, LinkEvent::RunState { run: current, phase: Phase::Interrupted, .. } if *current == run);
        apply(event, cx);
        if interrupted {
            break;
        }
    }

    pipe.handle.end(&session);
    assert!(pipe.link.join().expect("Shell link joined"));
    assert!(brain.join().expect("Brain joined"));
}

#[cfg(all(windows, debug_assertions))]
#[gpui::test]
#[expect(
    clippy::too_many_lines,
    reason = "both naturally completed turns share one real authenticated session"
)]
fn queued_authenticated_natural_completion_admits_a_fresh_typed_turn(cx: &mut TestAppContext) {
    use crate::link::{Batch, LinkEvent, fixture};
    use kumochi_protocol::message::{DataToShell, RunOrigin};
    use std::time::{Duration, Instant};

    let _device = fixture::serialize_playback();
    let (id, hello, probe, brain) =
        kumochi_brain::ipc::offline_plain_peer("0 text Distinct sound. End.".into(), 8);
    probe.release_ack();
    probe.release_run();
    probe.allow_data_credit();
    let pipe = fixture::authenticated_primed(id, hello);
    let events = fixture::patient(pipe.events);
    let owner = super::test_fixture::presence(cx);
    let session = fixture::connected(fixture::next(&events));
    owner.update(cx, |owner, cx| {
        owner.link = pipe.handle.clone();
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::Connected(session.clone())]),
                cx,
            )
            .unwrap();
    });
    let apply = |event, cx: &mut TestAppContext| {
        owner.update(cx, |owner, cx| {
            owner.link_events(Batch::Plain(vec![event]), cx).unwrap();
        });
    };
    let ready = Instant::now() + Duration::from_secs(3);
    let mut settings = false;
    while !settings || !pipe.handle.admits_text(Some(&session)) {
        assert!(
            Instant::now() < ready,
            "Presence applies Settings and the exact ACK arrives"
        );
        if let Ok(event) = events.recv_timeout(Duration::from_millis(5)) {
            settings |= matches!(&event, LinkEvent::Settings { .. });
            apply(event, cx);
        }
    }
    let mut prior = RunId(0);
    for (turn, draft) in [(1, "the first turn"), (2, "a fresh second turn")] {
        submit_draft(&owner, draft, cx);
        owner.read_with(cx, |owner, cx| {
            assert!(
                owner
                    .text
                    .as_ref()
                    .unwrap()
                    .input
                    .read(cx)
                    .value()
                    .is_empty(),
                "the exact session accepted typed turn {turn}; refusal = {:?}",
                owner.refusal
            );
        });
        let (mut named, mut done, mut ended) = (None, None, None);
        let (mut playback, mut rendered) = (None, false);
        let mut last_phase = None;
        let deadline = Instant::now() + Duration::from_secs(4);
        let run = loop {
            assert!(
                Instant::now() < deadline,
                "turn {turn} finishes without Esc: named={named:?}, phase={last_phase:?}, done={done:?}, ended={ended:?}, quiet={:?}, open={}, rendered={rendered}, playback={}, credits={:?}",
                probe.quiet_receipt(),
                session.open(),
                playback.is_some(),
                probe.data_credits()
            );
            for event in events.try_iter() {
                if let LinkEvent::RunState { phase, .. } = &event {
                    last_phase = Some(*phase);
                }
                match &event {
                    LinkEvent::RunState {
                        run,
                        phase: Phase::Thinking,
                        origin,
                        ..
                    } => {
                        assert_eq!(*origin, Some(RunOrigin::Typed { turn }));
                        named = Some(*run);
                    }
                    LinkEvent::RunState {
                        run,
                        phase: Phase::Done,
                        ..
                    } => done = Some(*run),
                    LinkEvent::Data {
                        message: DataToShell::RunEnd { run },
                        ..
                    } => ended = Some(*run),
                    _ => {}
                }
                apply(event, cx);
            }
            if let Some(run) = named
                && done == Some(run)
                && ended == Some(run)
                && probe.quiet() == run
            {
                break run;
            }
            if playback.is_none() {
                playback = pipe.playback.try_recv().ok();
            }
            if let Some(playback) = &mut playback
                && (playback.ready() || playback.stopped())
            {
                let mut samples = [0.0; 48];
                playback.render(&mut samples);
                if !rendered && samples.contains(&(15_430.0_f32 / 32_768.0)) {
                    if turn == 1 {
                        assert_eq!(
                            playback.handed(),
                            48,
                            "the first callback starts at source sample zero"
                        );
                    }
                    rendered = true;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        if turn == 1 {
            assert!(
                rendered,
                "the first naturally completed run rendered its source"
            );
            assert_heard_prefix(&probe, run);
            assert_eq!(
                probe.accepted_quiet(),
                None,
                "the natural quiet frame follows completed speech"
            );
        }
        assert!(run > prior);
        assert!(pipe.handle.admits_text(Some(&session)));
        prior = run;
    }
    let (received, applied) = probe.data_credits();
    assert!(applied > 0 && received >= applied);
    assert_eq!(probe.stop(), RunId(0), "both runs finished naturally");
    pipe.handle.end(&session);
    assert!(pipe.link.join().expect("Shell link joined"));
    assert!(brain.join().expect("Brain joined"));
}

#[cfg(all(windows, debug_assertions))]
fn assert_heard_prefix(
    probe: &kumochi_brain::ipc::AudioProbe,
    run: RunId,
) -> kumochi_protocol::message::Cursor {
    use kumochi_protocol::message::{Cursor, SegmentId};

    let retired = probe.retired(run);
    assert!(
        retired
            .iter()
            .any(|receipt| receipt.segment == SegmentId(0) && receipt.heard >= 48),
        "Brain accepted the presented source prefix from sample zero: {retired:?}"
    );
    let last = retired
        .iter()
        .rev()
        .find(|receipt| receipt.heard != 0)
        .expect("the Brain accepted a heard prefix");
    let cursor = Cursor {
        segment: last.segment,
        sample: last.heard,
    };
    assert_eq!(probe.quiet_receipt(), Some((run, Some(cursor))));
    cursor
}

fn escape(owner: &Entity<Presence>, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        owner.activity =
            Presence::activity(&owner.runs, super::EscapePanel::Closed, &owner.presenter);
        owner.escape(cx).expect("Esc");
    });
}

/// A brain queue that is full but still read.
fn full(
    owner: &Entity<Presence>,
    cx: &mut TestAppContext,
) -> async_channel::Receiver<ControlToBrain> {
    owner.update(cx, |owner, _| {
        let controls = owner.link.record_controls_bounded();
        let session = owner.session.clone().expect("connected");
        while owner.link.send_control(
            &session,
            ControlToBrain::Stop {
                run: RunId(1),
                revealed: Vec::new(),
            },
        ) {}
        controls
    })
}

#[gpui::test]
fn a_refused_stop_ends_the_session_and_never_stops_the_speaker(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let (commands, _tail) = answering(&owner, SPOKEN, cx);
    let _full = full(&owner, cx);
    escape(&owner, cx);
    cx.run_until_parked();
    owner.update(cx, |owner, _| {
        let alive = owner
            .session
            .as_ref()
            .is_some_and(|session| session.alive().load(std::sync::atomic::Ordering::Acquire));
        assert!(!alive, "the session ended");
    });
    assert_eq!(stops(&commands), [], "no speaker Stop");

    let (owner, _data, _controls) = ready(cx);
    let (commands, _tail) = answering(&owner, SPOKEN, cx);
    let _full = full(&owner, cx);
    let session = owner.update(cx, |owner, cx| {
        let session = owner.session.clone().expect("connected");
        let scenes = ControlToBrain::StopScenes {
            run: RunId(5),
            cuts: kumochi_protocol::message::SceneCuts::default(),
        };
        let effects = vec![crate::presenter::Effect::Control(scenes)];
        owner.scene_effects_at(Ok(effects), std::time::Instant::now(), cx);
        assert!(owner.session.is_none(), "the session is forgotten at once");
        assert!(
            session.alive().load(std::sync::atomic::Ordering::Acquire),
            "the end runs off the UI thread"
        );
        assert!(
            !owner.link.send_control(&session, brain_stop(6)),
            "but nothing more is sent at once"
        );
        session
    });
    cx.run_until_parked();
    assert!(
        !session.alive().load(std::sync::atomic::Ordering::Acquire),
        "the session ended"
    );
    assert_eq!(stops(&commands), [], "no speaker Stop");
}

#[gpui::test]
fn recognition_stops_a_spoken_run_the_gesture_s_pause_drained(cx: &mut TestAppContext) {
    let (owner, _data, controls) = ready(cx);
    let (commands, _tail) = answering(
        &owner,
        Snapshot {
            played: 480,
            ..SPOKEN
        },
        cx,
    );
    warm(&owner, 1, cx);
    hold(&owner, 1, cx);
    assert_eq!(brain_stops(&controls), [RunId(5)]);
    assert_eq!(stops(&commands), [RunId(5)]);
}

fn brain_stop(run: u64) -> ControlToBrain {
    ControlToBrain::Stop {
        run: RunId(run),
        revealed: Vec::new(),
    }
}

/// Queues the brain's and then the speaker's stop of each run in turn, after the owner has
/// refused the next `busy` offers.
fn stop_busy(owner: &Entity<Presence>, busy: u32, runs: &[u64], cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        owner.link.output().refuse(busy);
        let session = owner.session.clone().expect("connected");
        for run in runs {
            assert!(owner.send_stop(&session, brain_stop(*run), cx), "sent");
        }
    });
}

/// Lets the stop retry timer fire `times` times.
fn retries(times: u32, cx: &mut TestAppContext) {
    for _ in 0..times {
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(1));
        cx.run_until_parked();
    }
}

fn alive(owner: &Entity<Presence>, cx: &mut TestAppContext) -> bool {
    owner.update(cx, |owner, _| {
        owner
            .session
            .as_ref()
            .is_some_and(|session| session.alive().load(std::sync::atomic::Ordering::Acquire))
    })
}

#[gpui::test]
fn a_busy_owner_s_speaker_stop_is_offered_again_and_a_later_one_waits_behind_it(
    cx: &mut TestAppContext,
) {
    let (owner, _data, controls) = ready(cx);
    let (_commands, _tail) = answering(&owner, SPOKEN, cx);
    let seen = witness(&owner, &controls, cx);
    stop_busy(&owner, 1, &[5, 6], cx);
    retries(1, cx);
    assert_eq!(
        order(&seen, &controls),
        [
            ("brain", RunId(5)),
            ("brain", RunId(6)),
            ("speaker", RunId(5)),
            ("speaker", RunId(6))
        ]
    );
}

#[gpui::test]
fn a_speaker_stop_is_offered_ten_times_and_then_its_session_ends(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let (commands, _tail) = answering(&owner, SPOKEN, cx);
    stop_busy(&owner, 9, &[5], cx);
    retries(9, cx);
    assert_eq!(stops(&commands), [RunId(5)], "taken at the tenth offer");
    assert!(alive(&owner, cx));

    let (owner, _data, _controls) = ready(cx);
    let (commands, _tail) = answering(&owner, SPOKEN, cx);
    stop_busy(&owner, 10, &[5], cx);
    retries(8, cx);
    assert!(alive(&owner, cx), "one offer left");
    retries(1, cx);
    assert!(!alive(&owner, cx), "the session ended");
    assert_eq!(stops(&commands), [], "no speaker Stop");
}

#[gpui::test]
fn an_exhausted_stop_refuses_the_old_session_at_once_and_a_replacement_works(
    cx: &mut TestAppContext,
) {
    let (owner, _data, _controls) = ready(cx);
    let (_commands, _tail) = answering(&owner, SPOKEN, cx);
    stop_busy(&owner, 10, &[5], cx);
    retries(8, cx);
    owner.update(cx, |owner, cx| {
        let old = owner.session.clone().expect("connected");
        owner.stop_retry = None;
        owner.drain_stops(cx);
        assert!(
            old.alive().load(std::sync::atomic::Ordering::Acquire),
            "the end hasn't run yet"
        );
        assert!(!owner.link.send_control(&old, brain_stop(7)), "no control");
        assert!(
            !owner.link.send_data(
                &old,
                kumochi_protocol::message::DataToBrain::Credit { bytes: 1 }
            ),
            "no data"
        );
    });
    cx.run_until_parked();
    assert!(!alive(&owner, cx), "the session ended");
    owner.update(cx, |owner, _| {
        let replacement =
            crate::link::Session::detached(2, kumochi_protocol::message::Incarnation(2));
        assert!(owner.link.send_control(&replacement, brain_stop(8)));
    });
}

/// The pipe bytes of each step commit [`scene_step`] delivers.
const STEP_BYTES: u32 = 100;

/// Scene `scene` begins and run `run` commits its first step to it, as the data reader delivers
/// them, whether or not a `RunState` of that run was applied.
fn scene_step(owner: &Entity<Presence>, run: u64, scene: u64, cx: &mut TestAppContext) {
    use kumochi_protocol::message::{DataToShell, SceneHeader, SceneId};
    use kumochi_scene::{Record, deferred::Deferred, parse_record};
    let begin =
        format!(r#"{{"begin":{{"v":1,"scene":"s{scene}","surface":"overlay","alt":"test"}}}}"#);
    let Ok(Record::Begin(begin)) = parse_record(begin.as_bytes()) else {
        panic!("a begin record");
    };
    let step =
        br#"{"step":{"id":"s","put":[{"shape":{"id":"box","kind":"rect","box":[0,0,20,20]}}]}}"#;
    let Ok(Record::Step(step)) = parse_record(step) else {
        panic!("a step record");
    };
    let frames = [
        (
            0,
            DataToShell::SceneBegin {
                scene: SceneId(scene),
                header: SceneHeader {
                    begin,
                    layout_version: 2,
                },
            },
        ),
        (
            STEP_BYTES,
            DataToShell::SceneStepCommit {
                run: RunId(run),
                scene: SceneId(scene),
                index: 0,
                step,
                speech: None,
            },
        ),
    ];
    for (bytes, frame) in frames {
        let event = crate::link::LinkEvent::SceneData {
            generation: 1,
            frame: Deferred::new(frame, usize::try_from(bytes).expect("a few bytes")),
            bytes,
        };
        owner
            .update(cx, |owner, cx| {
                owner.link_events(crate::link::Batch::Plain(vec![event]), cx)
            })
            .expect("received");
    }
}

#[gpui::test]
fn recognition_fences_a_spoken_run_the_presenter_saw_before_its_run_state(cx: &mut TestAppContext) {
    kumochi_scene::deferred::start().expect("the dropper starts");
    let (owner, _data, controls) = ready(cx);
    let _kept = owner.update(cx, |owner, _| {
        owner.presenter.connected(1);
        owner
            .presenter
            .described(
                1,
                &kumochi_scene::text::Bounded::default(),
                std::time::Instant::now(),
            )
            .expect("empty membership");
        let (commands, tail) = owner.link.output().scripted(1);
        tail.publish(&Snapshot {
            spoken: 6,
            ..SPOKEN
        });
        (commands, tail)
    });
    scene_step(&owner, 6, 1, cx);
    owner.update(cx, |owner, _| {
        assert_eq!(owner.runs.stoppable(), None, "no RunState of run 6 yet");
        assert_eq!(
            owner.moving_run(std::time::Instant::now()),
            Some(RunId(6)),
            "the presenter's"
        );
    });
    warm(&owner, 1, cx);
    owner.update(cx, |owner, _| {
        assert_eq!(
            owner.voice.gates.run(GestureId(1)),
            Some((1, RunId(6))),
            "captured"
        );
    });
    hold(&owner, 1, cx);
    cx.run_until_parked();
    std::iter::from_fn(|| controls.try_recv().ok()).for_each(drop);
    let granted = owner.update(cx, |owner, _| {
        owner.session.as_ref().expect("connected").granted()
    });
    scene_step(&owner, 6, 2, cx);
    cx.run_until_parked();
    owner.update(cx, |owner, _| {
        assert_eq!(
            owner.runs.owned_disposition(RunId(6)),
            crate::runs::RunDisposition::Stopped
        );
        assert_eq!(
            owner.presenter.stoppable_run(),
            None,
            "late run 6 is skipped"
        );
        let applied = owner
            .runs
            .apply_state(RunId(6), ThreadId(1), Phase::Answering, None);
        assert!(!applied.visible, "its late RunState shows nothing");
    });
    let told: Vec<_> = std::iter::from_fn(|| controls.try_recv().ok()).collect();
    assert_eq!(
        told,
        [ControlToBrain::StepFailed {
            run: RunId(6),
            scene: kumochi_protocol::message::SceneId(2),
            index: 0,
            reason: kumochi_protocol::message::SceneFailure::Cancelled,
            cut: kumochi_scene::StopCut {
                activated: 0,
                revealed: kumochi_scene::text::Bounded::default(),
            },
        }],
        "skipped"
    );
    owner.update(cx, |owner, _| {
        assert_eq!(
            owner.session.as_ref().expect("connected").granted() - granted,
            STEP_BYTES,
            "and credited"
        );
    });
    scene_step(&owner, 7, 3, cx);
    owner.update(cx, |owner, _| {
        assert_eq!(
            owner.runs.owned_disposition(RunId(7)),
            crate::runs::RunDisposition::Live
        );
        assert_eq!(
            owner.presenter.stoppable_run(),
            Some(RunId(7)),
            "run 7 stays eligible"
        );
    });
}

/// Counts the microphones `owner` starts from now; each one fails to open.
fn starts(owner: &Entity<Presence>, cx: &mut TestAppContext) -> std::rc::Rc<std::cell::Cell<u32>> {
    let started = std::rc::Rc::<std::cell::Cell<u32>>::default();
    let count = std::rc::Rc::clone(&started);
    owner.update(cx, |owner, _| {
        owner.voice.start = Some(Box::new(move |_, _| {
            count.set(count.get() + 1);
            Err(crate::platform::microphone::MicError::Unavailable)
        }));
    });
    started
}

#[gpui::test]
fn a_hold_admitted_after_its_session_closed_never_opens_the_microphone(cx: &mut TestAppContext) {
    use kumochi_protocol::message::{HoldStatus, Incarnation};
    for refused in [false, true] {
        let (owner, _data, _controls) = ready(cx);
        let (_commands, tail) = answering(&owner, SPOKEN, cx);
        hold(&owner, 1, cx);
        let controls = if refused {
            Some(full(&owner, cx))
        } else {
            stop_busy(&owner, 10, &[5], cx);
            retries(8, cx);
            None
        };
        let started = starts(&owner, cx);
        owner.update(cx, |owner, cx| {
            let old = owner.session.clone().expect("connected");
            if refused {
                let scenes = ControlToBrain::StopScenes {
                    run: RunId(5),
                    cuts: kumochi_protocol::message::SceneCuts::default(),
                };
                assert!(!owner.send_stop(&old, scenes, cx), "refused");
            } else {
                owner.stop_retry = None;
                owner.drain_stops(cx);
            }
            assert!(
                old.alive().load(std::sync::atomic::Ordering::Acquire),
                "the end hasn't run yet"
            );
            owner
                .link_events(
                    crate::link::Batch::Plain(vec![crate::link::LinkEvent::VoiceHold {
                        generation: 1,
                        gesture: GestureId(1),
                        status: HoldStatus::Admitted,
                    }]),
                    cx,
                )
                .expect("voice status batch");
        });
        assert_eq!(started.get(), 0, "no microphone for the closed session");
        cx.run_until_parked();
        assert!(!alive(&owner, cx), "the session ended");

        drop(controls);
        let _controls = owner.update(cx, |owner, _| {
            let controls = owner.link.record_controls();
            owner.session = Some(crate::link::Session::detached(2, Incarnation(2)));
            owner.runs.met(Incarnation(2));
            owner.runs.connected(2);
            owner.runs.settings(ThreadId(1));
            // Session 1's speaker played out and released; the link thread opens session 2's
            // output with no speaker, as a reconnect does.
            tail.publish(&Snapshot {
                played: 480,
                flags: kumochi_shell_core::speaker::RELEASED,
                ..SPOKEN
            });
            owner.link.output().begin_quiet(2);
            let replacement = owner.session.clone().expect("reconnected");
            let replay = ControlToBrain::VoiceHoldStopped {
                gesture: GestureId(1),
            };
            assert!(
                owner.link.send_control(&replacement, replay),
                "the old receipt replays"
            );
            let sent_on = owner.link.link_generation();
            owner
                .link
                .accept_recorded_microphone_stop(sent_on, GestureId(1));
            controls
        });
        hold(&owner, 2, cx);
        let started = starts(&owner, cx);
        super::voice_wiring_tests::status(&owner, 2, 2, HoldStatus::Admitted, cx);
        assert_eq!(started.get(), 1, "the replacement's hold opens it");
    }
}

#[gpui::test]
fn a_refused_scene_control_of_any_kind_ends_its_session_once_and_a_replacement_works(
    cx: &mut TestAppContext,
) {
    let (owner, _data, _controls) = ready(cx);
    let controls = full(&owner, cx);
    let session = owner.update(cx, |owner, cx| {
        let session = owner.session.clone().expect("connected");
        let failed = ControlToBrain::SceneFailed {
            scene: kumochi_protocol::message::SceneId(3),
        };
        let effects = vec![crate::presenter::Effect::Control(failed)];
        owner.scene_effects_at(Ok(effects), std::time::Instant::now(), cx);
        assert!(owner.session.is_none(), "the session is forgotten at once");
        assert!(
            session.alive().load(std::sync::atomic::Ordering::Acquire),
            "the end runs off the UI thread"
        );
        while controls.try_recv().is_ok() {}
        assert!(
            !owner.link.send_control(&session, brain_stop(6)),
            "nothing more is sent, even with room"
        );
        session
    });
    cx.run_until_parked();
    assert!(
        !session.alive().load(std::sync::atomic::Ordering::Acquire),
        "the session ended"
    );
    owner.update(cx, |owner, _| {
        let _controls = owner.link.record_controls();
        let replacement =
            crate::link::Session::detached(2, kumochi_protocol::message::Incarnation(2));
        assert!(owner.link.send_control(&replacement, brain_stop(8)));
    });
}
