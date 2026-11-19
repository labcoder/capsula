//! A hold recognized over a spoken run stops that run, the brain's `Stop` first and the
//! speaker's only once it is queued; Esc keeps the same order. The speaker is scripted.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use gpui_kit::{Entity, TestAppContext};
use kumochi_protocol::message::{ControlToBrain, GestureId, Phase, RunId, ThreadId};

use super::Presence;
use super::voice::Stamp;
use super::voice_wiring_tests::{hold, ready};
use crate::input::TalkHeld;
use crate::input::hold::Intent;
use crate::strings::Text;
use kumochi_shell_core::speaker::{Command, Snapshot, Tail};

/// Run 5 answers on a live output that names it and has exposed `exposed` frames since. The
/// tail is kept, as a speaker's worker would keep it.
fn answering(
    owner: &Entity<Presence>,
    exposed: u64,
    cx: &mut TestAppContext,
) -> (Receiver<Command>, Arc<Tail>) {
    owner.update(cx, |owner, _| {
        owner
            .runs
            .apply_state(RunId(5), ThreadId(1), Phase::Answering, None);
        assert_eq!(owner.runs.stoppable(), Some(RunId(5)), "run 5 moves");
        let (commands, tail) = owner.link.output().scripted(1, RunId(5));
        assert!(matches!(commands.try_recv(), Ok(Command::Name(RunId(5)))));
        tail.publish(&Snapshot {
            exposed,
            ..Snapshot::default()
        });
        (commands, tail)
    })
}

fn warm(owner: &Entity<Presence>, gesture: u32, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
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
    let (_commands, _tail) = answering(&owner, 480, cx);
    let seen = witness(&owner, &controls, cx);
    warm(&owner, 1, cx);
    owner.update(cx, |owner, _| {
        assert_eq!(
            owner.voice.spoken,
            Some((GestureId(1), 1, RunId(5))),
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
    let (commands, _tail) = answering(&owner, 480, cx);
    warm(&owner, 1, cx);
    drop(controls);
    owner.update(cx, |owner, cx| {
        let gesture = GestureId(1);
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
    let (commands, _tail) = answering(&owner, 0, cx);
    warm(&owner, 1, cx);
    hold(&owner, 1, cx);
    assert_eq!(brain_stops(&controls), [], "the moving run keeps going");
    assert_eq!(stops(&commands), []);
}

#[gpui::test]
fn a_run_replaced_after_key_down_is_not_stopped(cx: &mut TestAppContext) {
    let (owner, _data, controls) = ready(cx);
    let (commands, _tail) = answering(&owner, 480, cx);
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
    let (_commands, _tail) = answering(&owner, 480, cx);
    let seen = witness(&owner, &controls, cx);
    escape(&owner, cx);
    assert_eq!(
        order(&seen, &controls),
        [("brain", RunId(5)), ("speaker", RunId(5))]
    );

    let (owner, _data, controls) = ready(cx);
    let (commands, _tail) = answering(&owner, 480, cx);
    drop(controls);
    escape(&owner, cx);
    assert_eq!(stops(&commands), [], "the brain wasn't told");
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
    let (commands, _tail) = answering(&owner, 480, cx);
    let _full = full(&owner, cx);
    escape(&owner, cx);
    owner.update(cx, |owner, _| {
        let alive = owner
            .session
            .as_ref()
            .is_some_and(|session| session.alive().load(std::sync::atomic::Ordering::Acquire));
        assert!(!alive, "the session ended");
    });
    assert_eq!(stops(&commands), [], "no speaker Stop");

    let (owner, _data, _controls) = ready(cx);
    let (commands, _tail) = answering(&owner, 480, cx);
    let _full = full(&owner, cx);
    owner.update(cx, |owner, _| {
        let session = owner.session.clone().expect("connected");
        let scenes = ControlToBrain::StopScenes {
            run: RunId(5),
            cuts: kumochi_protocol::message::SceneCuts::default(),
        };
        assert!(!owner.send_stop(&session, scenes), "refused");
        assert!(
            !session.alive().load(std::sync::atomic::Ordering::Acquire),
            "the session ended"
        );
    });
    assert_eq!(stops(&commands), [], "no speaker Stop");
}
