//! The worker that feeds the shared owner, against the engine's fake device clock.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use kumochi_protocol::message::{ControlToBrain, RunId};

use super::engine::tests::{GEN, R, engine, pcm, play, progress, queued, reports, stopped, write};
use super::{Command, Fault, Outlet, Packet, Speaker, late, launch, pair};

fn counted() -> (
    Outlet,
    Arc<AtomicU32>,
    async_channel::Receiver<ControlToBrain>,
) {
    let (control, brain) = async_channel::bounded(16);
    let granted = Arc::new(AtomicU32::new(0));
    let outlet = Outlet {
        generation: GEN,
        control,
        grant: Box::new({
            let granted = Arc::clone(&granted);
            move |bytes| {
                granted.fetch_add(bytes, Ordering::SeqCst);
            }
        }),
        tail: Arc::default(),
        wake: async_channel::bounded(1).0,
    };
    (outlet, granted, brain)
}

#[test]
fn the_worker_credits_each_frame_once_returns_its_buffer_and_holds_a_newer_run_uncredited() {
    let (outlet, granted, brain) = counted();
    let (mut speaker, mut worker) = pair(outlet);
    speaker.accept();
    let input = speaker.input().expect("the reader's side");
    let mut engine = engine();
    let send = |bytes, data| input.send(Packet { bytes, data }).expect("taken");
    send(10, queued(R, 0));
    assert!(worker.ingest(&mut engine));
    send(20, pcm(R, 0, 0, vec![5; 4], None));
    assert!(worker.ingest(&mut engine));
    assert_eq!(granted.load(Ordering::SeqCst), 30);
    let spent = input.spent.try_recv().expect("the buffer comes back");
    assert!(spent.is_empty(), "zeroed and cleared");
    write(&mut engine, 4);
    send(5, queued(RunId(8), 0));
    assert!(worker.ingest(&mut engine));
    assert_eq!(granted.load(Ordering::SeqCst), 30, "held, not credited");
    assert!(speaker.command(Command::Stop(R)));
    assert!(speaker.command(Command::Name(RunId(8))));
    assert!(worker.ingest(&mut engine));
    play(&mut engine, 4, 10).expect("plays");
    assert!(worker.ingest(&mut engine));
    assert_eq!(granted.load(Ordering::SeqCst), 35);
    worker.flush(&mut engine);
    assert_eq!(brain.try_recv(), Ok(progress(0, 4, 10)));
    assert_eq!(brain.try_recv(), Ok(stopped(R, Some((0, 4)))));
    assert_eq!(
        brain.try_recv(),
        Err(async_channel::TryRecvError::Empty),
        "nothing else"
    );
    drop(input);
    drop(speaker);
    assert!(!worker.ingest(&mut engine), "the link let go");
}

#[test]
fn a_run_older_than_the_named_one_is_credited_and_never_plays() {
    let (outlet, granted, _brain) = counted();
    let (mut speaker, mut worker) = pair(outlet);
    let input = speaker.input().expect("the reader's side");
    let mut engine = engine();
    assert!(speaker.command(Command::Name(RunId(8))));
    let send = |bytes, data| input.send(Packet { bytes, data }).expect("taken");
    send(10, queued(R, 0));
    assert!(worker.ingest(&mut engine));
    send(20, pcm(R, 0, 0, vec![5; 4], Some(4)));
    assert!(worker.ingest(&mut engine));
    assert_eq!(
        granted.load(Ordering::SeqCst),
        30,
        "dropped, and credited once each"
    );
    assert_eq!(write(&mut engine, 4), [0; 4]);
}

#[test]
fn data_waits_while_a_full_pass_of_commands_may_still_be_queued() {
    let (outlet, granted, _brain) = counted();
    let (mut speaker, mut worker) = pair(outlet);
    let input = speaker.input().expect("the reader's side");
    let mut engine = engine();
    for serial in 0..16 {
        assert!(speaker.command(Command::Resume(serial)));
    }
    assert!(
        !speaker.command(Command::Stop(R)),
        "a full queue refuses instead of waiting"
    );
    input
        .send(Packet {
            bytes: 10,
            data: queued(R, 0),
        })
        .expect("taken");
    assert!(worker.ingest(&mut engine));
    assert_eq!(granted.load(Ordering::SeqCst), 0, "not this pass");
    assert!(worker.ingest(&mut engine));
    assert_eq!(granted.load(Ordering::SeqCst), 10);
}

#[test]
fn a_device_that_starts_after_its_caller_gave_up_announces_nothing() {
    let (outlet, _granted, brain) = counted();
    assert!(late(outlet).is_none());
    assert_eq!(
        brain.try_recv(),
        Err(async_channel::TryRecvError::Closed),
        "its worker ended and published nothing"
    );
}

#[test]
fn a_device_that_failed_to_start_is_refused() {
    let (outlet, _granted, _brain) = counted();
    let refused = launch(outlet, Duration::from_secs(1), |_worker, ready| {
        let _told = ready.send(false);
    });
    assert!(refused.is_none());
}

#[test]
fn a_worker_that_stopped_reading_reports_lets_go() {
    let (outlet, _granted, _brain) = counted();
    let (_speaker, mut worker) = pair(outlet);
    let mut engine = engine();
    for run in 0..=512 {
        engine.stop(RunId(100 + run));
    }
    assert!(!worker.ingest(&mut engine));
}

#[test]
fn a_worker_whose_reports_back_up_after_a_device_loss_lets_go() {
    let (outlet, _granted, _brain) = counted();
    let (_speaker, mut worker) = pair(outlet);
    let mut engine = engine();
    engine.fail(Fault::Lost);
    for run in 0..511 {
        engine.stop(RunId(100 + run));
    }
    assert!(worker.ingest(&mut engine), "full, but not yet past it");
    engine.stop(RunId(611));
    assert!(!worker.ingest(&mut engine));
    assert_eq!(reports(&mut engine).len(), 512, "nothing more was queued");
}

#[test]
fn a_reader_waiting_on_a_busy_worker_gives_up_when_the_link_lets_go() {
    let (outlet, _granted, _brain) = counted();
    let (mut speaker, _worker) = pair(outlet);
    let input = speaker.input().expect("the reader's side");
    let packet = |bytes| Packet {
        bytes,
        data: queued(R, 0),
    };
    input.send(packet(1)).expect("the channel holds one");
    let reader = std::thread::spawn(move || input.send(packet(2)).is_err());
    std::thread::sleep(Duration::from_millis(30));
    drop(speaker);
    assert!(reader.join().expect("the reader returns"));
}

#[test]
fn letting_go_of_a_stuck_worker_returns_within_the_bound() {
    let (outlet, _granted, _brain) = counted();
    let (mut speaker, worker) = pair(outlet);
    let (release, stuck) = mpsc::channel::<()>();
    speaker.attach(std::thread::spawn(move || {
        let _held = worker;
        let _released = stuck.recv();
    }));
    assert!(lets_go_in_time(speaker));
    release.send(()).expect("the thread is still waiting");
}

/// Drops the speaker on another thread, so a wait that never ends fails the test instead of
/// hanging it.
fn lets_go_in_time(speaker: Speaker) -> bool {
    let (done, dropped) = mpsc::channel();
    std::thread::spawn(move || {
        drop(speaker);
        done.send(()).expect("the test waits");
    });
    dropped.recv_timeout(Duration::from_secs(2)).is_ok()
}

#[test]
fn letting_go_waits_for_the_thread_not_just_its_worker() {
    let (outlet, _granted, _brain) = counted();
    let (mut speaker, worker) = pair(outlet);
    let (release, stuck) = mpsc::channel::<()>();
    speaker.attach(std::thread::spawn(move || {
        drop(worker);
        let _released = stuck.recv();
    }));
    assert!(
        lets_go_in_time(speaker),
        "the thread outlived its worker, so it's left, not joined"
    );
    release.send(()).expect("the thread is still waiting");
}

#[test]
fn an_armed_worker_ends_the_connection_as_it_ends() {
    let (outlet, _granted, _brain) = counted();
    let (ended, end) = mpsc::channel();
    let (speaker, worker) = pair(outlet);
    speaker.arm(Box::new(move || ended.send(()).expect("the test waits")));
    std::thread::spawn(move || drop(worker))
        .join()
        .expect("the worker's thread ends");
    assert_eq!(end.try_recv(), Ok(()));
}

#[test]
fn arming_after_the_worker_ended_ends_the_connection_at_once() {
    let (outlet, _granted, _brain) = counted();
    let (ended, end) = mpsc::channel();
    let (speaker, worker) = pair(outlet);
    drop(worker);
    assert_eq!(
        end.try_recv(),
        Err(mpsc::TryRecvError::Empty),
        "not armed yet"
    );
    speaker.arm(Box::new(move || ended.send(()).expect("the test waits")));
    assert_eq!(end.try_recv(), Ok(()));
}

#[test]
fn a_packet_s_debug_shows_no_samples() {
    let packet = Packet {
        bytes: 9,
        data: pcm(R, 0, 0, vec![12_345; 3], None),
    };
    let shown = format!("{packet:?}");
    assert!(!shown.contains("12345"), "{shown}");
    assert!(!shown.contains("[57, 48"), "{shown}");
}
