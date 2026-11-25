//! A hold recognized over a spoken run stops that run, the brain's `Stop` first and the
//! speaker's only once it is queued; Esc keeps the same order. The speaker is scripted.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use gpui_kit::{Entity, TestAppContext};
use kumochi_protocol::message::{ControlToBrain, GestureId, Phase, RunId, RunOrigin, ThreadId};

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

#[gpui::test]
fn focused_escape_after_done_keeps_the_panel_and_sends_no_brain_stop(cx: &mut TestAppContext) {
    let (owner, _data, controls) = ready(cx);
    let (_commands, _tail) = answering(&owner, SPOKEN, cx);
    owner.update(cx, |owner, cx| {
        let text = owner.text.as_mut().expect("typing panel");
        text.typing.open();
        text.focused = true;
        owner
            .runs
            .apply_state(RunId(5), ThreadId(1), Phase::Done, None);
        owner.speech.set(Some((
            1,
            RunId(5),
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        )));
        owner.focus_changed(cx);
        assert!(owner.runs.state_ended(RunId(5)));
        assert_eq!(owner.runs.stoppable(), Some(RunId(5)), "RunEnd is pending");
        owner.escape(cx).expect("Esc");
        assert!(owner.text_focused());
        assert_eq!(
            owner.runs.stoppable(),
            Some(RunId(5)),
            "a speaker-only stop does not fence the Brain run"
        );
    });
    assert_eq!(brain_stops(&controls), []);
}

#[gpui::test]
fn a_queued_panel_escape_stops_only_the_speech_it_saw(cx: &mut TestAppContext) {
    use crate::platform::PresenceEvent;
    use std::time::{Duration, Instant};

    super::test_fixture::without_desktop(cx);
    for (successor_already_named, closed) in [(false, false), (true, false), (true, true)] {
        let (owner, _data, controls) = ready(cx);
        let (_commands, _tail) = answering(&owner, SPOKEN, cx);
        let (events, queued) = async_channel::unbounded();
        let until = Instant::now() + Duration::from_secs(1);
        owner.update(cx, |owner, cx| {
            let text = owner.text.as_mut().expect("typing panel");
            text.typing.open();
            text.focused = true;
            owner.speech.set(Some((1, RunId(5), until)));
            owner.focus_changed(cx);
        });
        cx.update(|cx| super::send_escape(&events, Some(&owner.downgrade()), cx));
        let event = queued.try_recv().expect("one Esc was queued");
        assert!(matches!(event, PresenceEvent::EscapeSpeech(press) if press.run == RunId(5)));
        assert!(queued.is_empty());
        owner.update(cx, |owner, cx| {
            owner.runs.submit("next", |_| true).expect("admitted turn");
            if successor_already_named {
                owner.runs.apply_state(
                    RunId(6),
                    ThreadId(1),
                    Phase::Answering,
                    Some(RunOrigin::Typed { turn: 1 }),
                );
                owner.speech.set(Some((1, RunId(6), until)));
            }
            if closed {
                owner.session.as_ref().expect("original pair").close();
            }
            owner.on_event(event, cx).expect("deferred Esc");
            if !successor_already_named {
                let applied = owner.runs.apply_state(
                    RunId(6),
                    ThreadId(1),
                    Phase::Answering,
                    Some(RunOrigin::Typed { turn: 1 }),
                );
                assert!(applied.visible, "the unnamed turn wasn't cancelled");
                assert_eq!(applied.stop, None);
            }
            assert_eq!(owner.runs.stoppable(), Some(RunId(6)));
            assert!(owner.text_focused());
        });
        let expected = if successor_already_named {
            vec![]
        } else {
            vec![RunId(5)]
        };
        assert_eq!(brain_stops(&controls), expected);
    }
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
pub(super) fn submit_draft(owner: &Entity<Presence>, draft: &'static str, cx: &mut TestAppContext) {
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
    use kumochi_protocol::message::{
        Cursor, DataToShell, OutputGeneration, SegmentId, SpeechRetirement,
    };
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    const WAIT: Duration = Duration::from_secs(3);
    const CALLBACK_PERIOD: Duration = Duration::from_millis(2);
    const PCM: [u32; 48] = [(15_430.0_f32 / 32_768.0).to_bits(); 48];
    const PREFIX: u64 = 480;
    const FINAL: u64 = PREFIX + 48;
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
        Err(mpsc::TryRecvError::Empty)
    ));
    let mut playback = std::thread::scope(|scope| {
        let (handoff, listen) = mpsc::channel();
        let (presented, confirm) = mpsc::channel();
        let pump = scope.spawn(move || {
            let mut playback = pipe
                .playback
                .recv_timeout(WAIT)
                .expect("the real worker opened its fake device");
            let limit = Instant::now() + WAIT;
            let mut notice = Some(presented);
            let mut requested = false;
            let mut samples = [0.0; 48];
            loop {
                assert!(Instant::now() < limit, "Esc takes the original fake device");
                assert!(!playback.stopped(), "the original fake device stays live");
                let settled = playback.presented_prefix();
                if settled == PREFIX
                    && let Some(notice) = notice.take()
                {
                    notice
                        .send(())
                        .expect("Presence waits for due presentation");
                }
                if requested && settled == FINAL {
                    return playback;
                }
                let before = playback.handed();
                if before < PREFIX || (requested && before == PREFIX) {
                    playback.render(&mut samples);
                    let handed = playback.handed();
                    if handed != before {
                        assert_eq!(handed, before + 48, "source advances by one exact block");
                        assert_eq!(samples.map(f32::to_bits), PCM);
                    }
                } else {
                    playback.render(&mut []);
                }
                let next = Instant::now() + CALLBACK_PERIOD;
                loop {
                    match listen.recv_timeout(next.saturating_duration_since(Instant::now())) {
                        Ok(()) => {
                            assert_eq!(playback.presented_prefix(), PREFIX);
                            requested = true;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => return playback,
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                    }
                }
            }
        });
        probe.release_run();
        let (mut answering, mut drawn, mut presented) = (false, false, false);
        let limit = Instant::now() + WAIT;
        loop {
            let blocked = probe.data_blocked();
            let credits = probe.data_credits();
            presented |= confirm.try_recv().is_ok();
            assert!(
                session.open() && Instant::now() < limit && !pump.is_finished(),
                "still waiting for Esc: answering={answering} drawn={drawn} blocked={blocked} credits={credits:?} presented={presented} open={} pump_done={}",
                session.open(),
                pump.is_finished()
            );
            assert_eq!(credits.1, 0);
            if answering && drawn && blocked && credits.0 > 0 && presented {
                break;
            }
            match events.recv_timeout(CALLBACK_PERIOD) {
                Ok(event) => {
                    answering |= matches!(&event, LinkEvent::RunState { run: current, phase: Phase::Answering, .. } if *current == run);
                    drawn |= matches!(&event, LinkEvent::Data { message: DataToShell::StepCommit { run: current, .. }, .. } if *current == run);
                    apply(event, cx);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("events disconnected: answering={answering} drawn={drawn}");
                }
            }
        }
        owner.read_with(cx, |owner, _| {
            assert!(owner.runs.drawing());
            assert_eq!(owner.runs.stoppable(), Some(run));
        });
        assert!(!pipe.handle.admits_text(Some(&session)));
        let mut playback = None;
        fixture::with_outbox_held(&pipe.handle, &session, || {
            handoff.send(()).expect("the callback pump is still active");
            let playback =
                playback.insert(pump.join().expect("the callback pump returns its device"));
            assert!(session.open());
            assert!(!playback.stopped());
            assert_eq!(playback.handed(), FINAL);
            assert_eq!(playback.presented_prefix(), FINAL);
            assert!(probe.data_blocked());
            let (returned, applied) = probe.data_credits();
            assert!(returned > 0);
            assert_eq!(applied, 0);
            assert_eq!(probe.stop(), RunId(0));
            assert!(probe.received_retirements(run).is_empty());
            assert!(probe.retired(run).is_empty());
            escape(&owner, cx);
            owner.read_with(cx, |owner, _| {
                assert_eq!(
                    owner.runs.stoppable(),
                    None,
                    "Esc fenced the drawn run while the exact outbox was still locked"
                );
            });
        });
        playback.expect("Esc takes the original fake device")
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
    assert_eq!(probe.accepted_quiet(), None);
    assert!(
        !pipe.handle.admits_text(Some(&session)),
        "the unconfirmed tail still owns output"
    );
    let mut after = [99.0; 128];
    playback.render(&mut after);
    assert!(after[0] > 0.0 && after[0] < f32::from_bits(PCM[0]));
    assert!(after[119..].iter().all(|sample| sample.to_bits() == 0));
    playback.render(&mut after);
    assert_eq!(after.map(f32::to_bits), [0; 128]);
    wait("Brain accepts the original run's quiet receipt", || {
        probe
            .accepted_quiet()
            .is_some_and(|(stopped, _)| stopped == run)
    });
    let retirement = SpeechRetirement {
        run,
        output: OutputGeneration(1),
        segment: SegmentId(0),
        retired: 528,
        heard: 528,
        final_segment: false,
    };
    let quiet = Some((
        run,
        Some(Cursor {
            segment: SegmentId(0),
            sample: 528,
        }),
    ));
    assert_eq!(probe.received_retirements(run), [retirement]);
    assert_eq!(probe.retired(run), [retirement]);
    assert_eq!(probe.quiet_receipt(), quiet);
    assert_eq!(probe.accepted_quiet(), quiet);
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
        submit_draft(&owner, "while it answers", cx);
        owner.read_with(cx, |owner, _| {
            assert_eq!(
                owner.refusal,
                Some(Text::TurnBusy),
                "Enter during turn {turn}'s reply says the pair is busy"
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
            if let Some(playback) = &mut playback {
                let proving_start =
                    turn == 1 && playback.handed() == 48 && playback.presented_prefix() < 48;
                if proving_start && !playback.stopped() {
                    playback.render(&mut []);
                } else if playback.ready() || playback.stopped() {
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

#[cfg(all(windows, debug_assertions))]
#[gpui::test]
#[expect(
    clippy::too_many_lines,
    reason = "these spoken turns share one real authenticated pair"
)]
fn queued_speech_holds_esc_only_while_presented_and_never_from_a_focused_box(
    cx: &mut TestAppContext,
) {
    use crate::escape::{SpeechEscapeOwner, SpeechPress};
    use crate::link::{Batch, LinkEvent, fixture};
    use kumochi_protocol::message::DataToShell;
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
    let settings = std::cell::Cell::new(false);
    // Some while run ends are held back, so a run's Done is in before its RunEnd.
    let run_ends = std::cell::RefCell::new(None::<Vec<LinkEvent>>);
    let apply = |cx: &mut TestAppContext| {
        for event in events.try_iter() {
            settings.set(settings.get() || matches!(&event, LinkEvent::Settings { .. }));
            if let Some(held) = run_ends.borrow_mut().as_mut()
                && matches!(
                    &event,
                    LinkEvent::Data {
                        message: DataToShell::RunEnd { .. },
                        ..
                    }
                )
            {
                held.push(event);
                continue;
            }
            owner.update(cx, |owner, cx| {
                owner.link_events(Batch::Plain(vec![event]), cx).unwrap();
            });
        }
        // The owner's wakes come from foreign threads, which the test scheduler refuses, so this
        // steps the dispatcher they would poke.
        owner.update(cx, Presence::dispatch);
        cx.run_until_parked();
    };
    let wait =
        |what: &str, cx: &mut TestAppContext, done: &mut dyn FnMut(&mut TestAppContext) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(4);
            while !done(cx) {
                assert!(Instant::now() < deadline, "{what}");
                apply(cx);
                std::thread::sleep(Duration::from_millis(2));
            }
        };
    let held =
        |cx: &mut TestAppContext| owner.read_with(cx, |owner, _| owner.escape_hold.shows_stop());
    let lease = || pipe.handle.output().speech(session.generation());
    wait(
        "Presence applies Settings and the pair is ready",
        cx,
        &mut |_| settings.get() && pipe.handle.admits_text(Some(&session)),
    );
    let mut block = [0.0; 48];
    let mut first: Option<SpeechPress> = None;
    for (turn, focused) in [(1, false), (2, true), (3, false), (4, true), (5, false)] {
        owner.update(cx, |owner, cx| {
            let text = owner.text.as_mut().unwrap();
            text.focused = false;
            if focused {
                text.typing.open();
            } else {
                text.typing.close(super::text::Hide::Dismiss);
            }
            owner.focus_changed(cx);
        });
        if matches!(turn, 1 | 4) {
            *run_ends.borrow_mut() = Some(Vec::new());
        }
        fixture::prime_next_playback(&pipe.handle);
        submit_draft(&owner, "speak", cx);
        if turn == 2 {
            let stopped = probe.stop();
            owner.update(cx, |owner, cx| {
                let run = first.expect("the first run's lease").run;
                assert!(
                    owner.runs.state_ended(run) && owner.runs.stoppable() == Some(run),
                    "the first run told Done and its RunEnd is held"
                );
                owner.native_escape(None, first, cx).expect("Esc");
            });
            assert_eq!(
                probe.stop(),
                stopped,
                "a press on a run that told Done sends the Brain no Stop"
            );
            for event in run_ends.take().expect("held run ends") {
                owner.update(cx, |owner, cx| {
                    owner.link_events(Batch::Plain(vec![event]), cx).unwrap();
                });
            }
            owner.read_with(cx, |owner, _| {
                assert_eq!(owner.runs.stoppable(), None);
                assert!(
                    owner.runs.working(),
                    "the turn sent after that speech still waits for its answer"
                );
            });
        }
        let mut playback = None;
        wait("the run's device opens", cx, &mut |_| {
            playback = playback.take().or_else(|| pipe.playback.try_recv().ok());
            playback
                .as_ref()
                .is_some_and(fixture::FixturePlayback::ready)
        });
        let mut device = playback.unwrap();
        if turn == 1 {
            assert!(
                !pipe.handle.admits_text(Some(&session)),
                "the bound run already owes its quiet"
            );
            let before = std::iter::repeat_with(lease)
                .find(Result::is_ok)
                .expect("the owner frees up");
            assert_eq!(before, Ok(None), "no PCM was presented yet");
            assert!(!held(cx), "Esc isn't taken before the first PCM");
        }
        wait("presented PCM gives a speech lease", cx, &mut |_| {
            if matches!(lease(), Ok(None)) {
                device.render(&mut block);
            }
            matches!(lease(), Ok(Some(_)))
        });
        wait("the shell reads the lease", cx, &mut |cx| {
            owner.read_with(cx, |owner, _| {
                SpeechPress::capture(
                    owner.speech.get(),
                    Instant::now(),
                    SpeechEscapeOwner::Outside,
                )
                .is_some()
            })
        });
        match turn {
            1 => {
                assert!(held(cx), "presented speech takes Esc");
                first = owner.read_with(cx, |owner, _| {
                    SpeechPress::capture(
                        owner.speech.get(),
                        Instant::now(),
                        SpeechEscapeOwner::Outside,
                    )
                });
                let stopped = probe.stop();
                owner.update(cx, |owner, cx| {
                    let press = first.expect("a lease");
                    let run = press.run;
                    owner
                        .native_escape(
                            None,
                            Some(SpeechPress {
                                session: press.session + 1,
                                ..press
                            }),
                            cx,
                        )
                        .expect("Esc");
                    assert_eq!(
                        owner.runs.stoppable(),
                        Some(run),
                        "a press that saw another session's speech stops nothing here"
                    );
                });
                assert_eq!(probe.stop(), stopped, "and tells the Brain nothing");
                wait("the reply finishes", cx, &mut |_| {
                    device.render(&mut block);
                    pipe.handle.admits_text(Some(&session))
                });
                std::thread::sleep(Duration::from_millis(300));
                cx.executor().advance_clock(Duration::from_secs(1));
                apply(cx);
                assert!(matches!(lease(), Ok(None)));
                assert!(!held(cx), "Esc goes back once the evidence runs out");
                wait("the first run's RunEnd is held", cx, &mut |_| {
                    run_ends
                        .borrow()
                        .as_ref()
                        .is_some_and(|held| !held.is_empty())
                });
            }
            2 => {
                assert!(held(cx), "an open box without focus leaves Esc to speech");
                owner.update(cx, |owner, cx| {
                    owner.text.as_mut().unwrap().focused = true;
                    owner.focus_changed(cx);
                });
                assert!(!held(cx), "the focused box keeps Esc while it speaks");
                owner.update(cx, |owner, cx| owner.escape(cx).expect("Esc"));
                wait("the box's Esc stops the speaker", cx, &mut |_| {
                    device.stopped()
                });
                wait("the stopped reply goes quiet", cx, &mut |_| {
                    device.render(&mut block);
                    pipe.handle.admits_text(Some(&session))
                });
                owner.read_with(cx, |owner, _| {
                    assert!(
                        owner.text_open(),
                        "the press that stopped speech left the box"
                    );
                });
                owner.update(cx, |owner, cx| owner.escape(cx).expect("Esc"));
                owner.read_with(cx, |owner, _| {
                    assert!(!owner.text_open(), "the next press reaches the box");
                });
            }
            3 => {
                let stopped = probe.stop();
                owner.update(cx, |owner, cx| {
                    let successor = owner.runs.stoppable();
                    owner.native_escape(None, first, cx).expect("Esc");
                    assert_eq!(
                        owner.runs.stoppable(),
                        successor,
                        "a press that saw the first run's speech leaves its successor running"
                    );
                });
                std::thread::sleep(Duration::from_millis(50));
                apply(cx);
                assert!(!device.stopped(), "the successor keeps speaking");
                assert_eq!(probe.stop(), stopped, "and the Brain gets no Stop");
                wait("the successor finishes", cx, &mut |_| {
                    device.render(&mut block);
                    pipe.handle.admits_text(Some(&session))
                });
                pipe.handle.output().refuse(1000);
                owner.update(cx, |owner, cx| {
                    owner.native_escape(None, first, cx).expect("Esc");
                });
                wait("the local stop is given up", cx, &mut |cx| {
                    cx.executor().advance_clock(Duration::from_millis(5));
                    owner.read_with(cx, |owner, _| owner.pending_stops.is_empty())
                });
                pipe.handle.output().refuse(0);
                assert!(
                    session.open(),
                    "a local stop that can't get through never ends the pair"
                );
            }
            4 => {
                let stopped = probe.stop();
                owner.update(cx, |owner, cx| {
                    owner.text.as_mut().unwrap().focused = true;
                    owner.focus_changed(cx);
                    let (_, run, _) = owner.speech.get().expect("current speech");
                    let thread = owner.runs.thread().expect("current thread");
                    owner.runs.apply_state(run, thread, Phase::Done, None);
                    assert!(owner.runs.state_ended(run));
                    assert_eq!(owner.runs.stoppable(), Some(run), "RunEnd is held");
                    owner.escape(cx).expect("focused Esc");
                    assert!(owner.text_focused());
                });
                wait(
                    "the focused terminal press stops the output",
                    cx,
                    &mut |_| device.stopped(),
                );
                assert_eq!(
                    probe.stop(),
                    stopped,
                    "the local stop told the Brain nothing"
                );
                wait("the terminal reply goes quiet", cx, &mut |_| {
                    device.render(&mut block);
                    pipe.handle.admits_text(Some(&session))
                });
                for event in run_ends.take().expect("held run ends") {
                    owner.update(cx, |owner, cx| {
                        owner.link_events(Batch::Plain(vec![event]), cx).unwrap();
                    });
                }
                assert!(session.open());
            }
            _ => {
                assert!(held(cx));
                device.lose();
                wait("the lost route is retired", cx, &mut |_| device.stopped());
                assert_eq!(
                    std::iter::repeat_with(lease).find(Result::is_ok),
                    Some(Ok(None)),
                    "retiring the route ended the lease at once"
                );
                wait("a lost route gives Esc back", cx, &mut |cx| !held(cx));
            }
        }
        drop(device);
        if turn < 5 {
            fixture::await_fake_disposal();
        }
    }
    pipe.handle.end(&session);
    assert!(pipe.link.join().expect("Shell link joined"));
    assert!(brain.join().expect("Brain joined"));
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

#[cfg(all(windows, debug_assertions))]
#[gpui::test]
fn queued_escape_stops_audio_before_teardown_when_the_control_queue_is_full(
    cx: &mut TestAppContext,
) {
    use crate::escape::{SpeechEscapeOwner, SpeechPress};
    use crate::link::{Batch, LinkEvent, fixture};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    let _device = fixture::serialize_playback();
    let script = "0 text Distinct sound. End.\n5000 text Later.";
    let (id, hello, probe, brain) = kumochi_brain::ipc::offline_plain_peer(script.into(), 128);
    probe.release_ack();
    probe.release_run();
    probe.allow_data_credit();
    let pipe = fixture::authenticated_primed(id, hello);
    let events = fixture::patient(pipe.events);
    let session = fixture::connected(fixture::next(&events));
    let owner = super::test_fixture::presence(cx);
    owner.update(cx, |owner, _| owner.link = pipe.handle.clone());
    let apply = |cx: &mut TestAppContext, event| {
        owner.update(cx, |owner, cx| {
            owner.link_events(Batch::Plain(vec![event]), cx).unwrap();
        });
    };
    apply(cx, LinkEvent::Connected(session.clone()));
    let until = |cx: &mut TestAppContext, done: &mut dyn FnMut(&mut TestAppContext) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !done(cx) {
            assert!(session.open() && Instant::now() < deadline);
            for event in events.try_iter() {
                apply(cx, event);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    let mut settings = false;
    until(cx, &mut |cx| {
        settings |= owner.read_with(cx, |owner, cx| owner.settings.read(cx).view().is_some());
        settings && pipe.handle.admits_text(Some(&session))
    });
    submit_draft(&owner, "speak", cx);
    let mut playback = pipe.playback.recv_timeout(Duration::from_secs(3)).unwrap();
    let mut samples = [0.0; 48];
    let mut heard = false;
    let mut press = None;
    until(cx, &mut |cx| {
        playback.render(&mut samples);
        heard |= playback.handed() != 0 && samples.iter().any(|sample| *sample != 0.0);
        press = owner.update(cx, |owner, cx| {
            owner.refresh_speech(cx);
            SpeechPress::capture(
                owner.speech.get(),
                Instant::now(),
                SpeechEscapeOwner::Outside,
            )
            .filter(|press| owner.runs.stoppable() == Some(press.run))
        });
        heard && press.is_some()
    });
    assert!(!playback.stopped());
    let full = full(&owner, cx);
    assert_eq!(Some(full.len()), full.capacity());
    owner.update(cx, |owner, cx| {
        owner.native_escape(None, press, cx).expect("Esc");
    });
    assert!(!session.open(), "the failed enqueue seals the pair");
    let alive = session.alive();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !playback.stopped() {
        assert!(
            alive.load(Ordering::Acquire) && Instant::now() < deadline,
            "the speaker must take Stop before background session teardown"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        alive.load(Ordering::Acquire),
        "background teardown hasn't run"
    );
    assert_eq!(probe.stop(), RunId(0), "no Stop reached the brain");
    cx.run_until_parked();
    assert!(!alive.load(Ordering::Acquire));
    assert!(pipe.link.join().unwrap());
    assert!(brain.join().unwrap());
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
