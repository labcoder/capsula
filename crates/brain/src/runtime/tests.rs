use std::time::Duration;

use capsula_protocol::frame::{self, Pipe};
use capsula_protocol::message::{
    ControlToBrain, ControlToShell, DataToBrain, DataToShell, Phase, RunId, Step, ThreadId,
};
use tokio::io::{AsyncWriteExt as _, ReadHalf, WriteHalf};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer};
use tokio::time::timeout;

use super::fixture::Fixture;
use super::{Brain, Ended, MAX_ENDING};
use crate::ipc;
use crate::platform::{self, Instance};
use crate::store::Store;
use crate::tap::Tap;
use crate::test_support::{self, Scratch};

/// A connected server and client for one pipe, under a name no running brain uses.
async fn pair(name: &str) -> (NamedPipeServer, NamedPipeClient) {
    let name = format!(r"\\.\pipe\capsula-test-{}-{name}", std::process::id());
    let server = platform::create_owner_only(&name, Instance::First).expect("server");
    let client = ClientOptions::new().open(&name).expect("client");
    server.connect().await.expect("connected");
    (server, client)
}

/// The test's side of a shell: it reads control, and data only when asked.
struct Shell {
    control_in: ReadHalf<NamedPipeClient>,
    control_out: WriteHalf<NamedPipeClient>,
    data_in: ReadHalf<NamedPipeClient>,
    data_out: WriteHalf<NamedPipeClient>,
    /// The thread of the last run state read.
    thread: Option<ThreadId>,
}

impl Shell {
    async fn submit(&mut self, text: &str) {
        let message = DataToBrain::TextSubmit {
            thread: None,
            text: text.to_owned(),
        };
        let frame = frame::encode(&message, Pipe::Data).expect("encodes");
        self.data_out.write_all(&frame).await.expect("sent");
    }

    async fn stop(&mut self, run: RunId) {
        let message = ControlToBrain::Stop {
            run,
            revealed: Vec::new(),
        };
        let frame = frame::encode(&message, Pipe::Control).expect("encodes");
        self.control_out.write_all(&frame).await.expect("sent");
    }

    /// The next run state, answering pings on the way.
    async fn state(&mut self) -> (RunId, Phase) {
        loop {
            let message = ipc::read::<ControlToShell>(&mut self.control_in, Pipe::Control)
                .await
                .expect("readable")
                .expect("open");
            match message {
                ControlToShell::RunState { run, thread, phase } => {
                    self.thread = Some(thread);
                    return (run, phase);
                }
                ControlToShell::Ping => {
                    let pong =
                        frame::encode(&ControlToBrain::Pong, Pipe::Control).expect("encodes");
                    self.control_out.write_all(&pong).await.expect("sent");
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    async fn data(&mut self) -> DataToShell {
        ipc::read::<DataToShell>(&mut self.data_in, Pipe::Data)
            .await
            .expect("readable")
            .expect("open")
    }
}

async fn connect(name: &str) -> ((NamedPipeServer, NamedPipeServer), Shell) {
    let (control, control_client) = pair(&format!("{name}-control")).await;
    let (data, data_client) = pair(&format!("{name}-data")).await;
    let (control_in, control_out) = tokio::io::split(control_client);
    let (data_in, data_out) = tokio::io::split(data_client);
    (
        (control, data),
        Shell {
            control_in,
            control_out,
            data_in,
            data_out,
            thread: None,
        },
    )
}

#[tokio::test]
async fn stop_is_served_while_the_shell_reads_no_data() {
    let scratch = Scratch::new("saturated");
    // Far more text than the pipe buffer and the credit window hold, so the data writer is
    // blocked when Stop arrives.
    let script = "0 fill 240000\n0 fill 240000\n0 fill 240000\n0 fill 240000\n5000 step late";
    let mut brain = test_support::brain(&scratch, script);
    let ((control, data), mut shell) = connect("saturated").await;
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("go").await;
        let (run, thinking) = shell.state().await;
        assert_eq!(thinking, Phase::Thinking);
        assert_eq!(shell.state().await, (run, Phase::Answering));
        // Let the writer fill the pipe while nothing reads it.
        tokio::time::sleep(Duration::from_millis(200)).await;
        shell.stop(run).await;
        let interrupted = timeout(Duration::from_millis(500), shell.state()).await;
        assert_eq!(
            interrupted.expect("served promptly"),
            (run, Phase::Interrupted)
        );
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close();
}

#[tokio::test]
async fn a_step_finished_after_stop_never_reaches_the_shell_or_the_store() {
    let scratch = Scratch::new("late");
    let mut brain = test_support::brain(
        &scratch,
        "0 text a\n0 step one\n3000 step never\nlate closed",
    );
    let ((control, data), mut shell) = connect("late").await;
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("go").await;
        let (run, _) = shell.state().await;
        let first_thread = shell.thread.expect("a run state names its thread");
        assert_eq!(
            shell.data().await,
            DataToShell::AssistantText {
                run,
                delta: "a".to_owned()
            }
        );
        assert_eq!(
            shell.data().await,
            DataToShell::StepCommit {
                run,
                index: 0,
                step: Step {
                    label: "one".to_owned()
                }
            }
        );
        shell.stop(run).await;
        // Answering came with the text; Interrupted follows the Stop.
        assert_eq!(shell.state().await, (run, Phase::Answering));
        assert_eq!(shell.state().await, (run, Phase::Interrupted));
        // A replacement run right after: the old run's late step must not show up before it.
        shell.submit("again").await;
        let (next, phase) = shell.state().await;
        assert!(next > run, "{next:?} after {run:?}");
        assert_eq!(phase, Phase::Thinking);
        assert_eq!(shell.data().await, DataToShell::RunEnd { run });
        let first = timeout(Duration::from_millis(500), shell.data()).await;
        assert_eq!(
            first.expect("the new run streams"),
            DataToShell::AssistantText {
                run: next,
                delta: "a".to_owned()
            }
        );
        drop(shell);
        first_thread
    };
    let (ended, first_thread) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close();
    let store = Store::open(&scratch.db()).expect("reopen");
    assert!(store.search("closed", 10).expect("search").is_empty());
    // The replacement run may or may not commit its own step before the shell hangs up.
    assert!(
        store
            .search("one", 10)
            .expect("search")
            .contains(&first_thread)
    );
}

#[tokio::test]
async fn a_runs_data_is_complete_at_its_run_end_not_at_its_done() {
    let scratch = Scratch::new("runend");
    let mut brain = test_support::brain(&scratch, "0 text a\n0 step one\n0 text b");
    let ((control, data), mut shell) = connect("runend").await;
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("go").await;
        // Done can arrive on control before any of the data is read: the pipes aren't ordered.
        let (run, _) = shell.state().await;
        assert_eq!(shell.state().await, (run, Phase::Answering));
        assert_eq!(shell.state().await, (run, Phase::Done));
        let mut frames = Vec::new();
        loop {
            let frame = timeout(Duration::from_secs(2), shell.data())
                .await
                .expect("the run's data ends with its RunEnd");
            let last = frame == DataToShell::RunEnd { run };
            frames.push(frame);
            if last {
                break;
            }
        }
        let text = |delta: &str| DataToShell::AssistantText {
            run,
            delta: delta.to_owned(),
        };
        assert_eq!(
            frames,
            [
                text("a"),
                DataToShell::StepCommit {
                    run,
                    index: 0,
                    step: Step {
                        label: "one".to_owned()
                    }
                },
                text("b"),
                DataToShell::RunEnd { run },
            ]
        );
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close();
}

#[tokio::test]
async fn new_turns_wait_while_too_many_runs_wait_on_the_store() {
    let scratch = Scratch::new("backlog");
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    let high_water = store.high_water().expect("high water");
    // Another connection holds the write lock, so every store write waits on it.
    store
        .conn_for_tests()
        .busy_timeout(Duration::from_secs(10))
        .expect("busy timeout");
    let lock = rusqlite::Connection::open(scratch.db()).expect("lock");
    lock.execute_batch("BEGIN IMMEDIATE").expect("locked");
    let fixture = Fixture::parse("0 text a").expect("script");
    let mut brain = Brain::new(store, high_water, fixture).expect("brain");
    let ((control, data), mut shell) = connect("backlog").await;
    let mut tap = Tap::from_env();
    let turns = MAX_ENDING + 4;
    let client = async {
        for _ in 0..turns {
            shell.submit("go").await;
        }
        let mut thinking = 0;
        while let Ok((_, phase)) = timeout(Duration::from_millis(300), shell.state()).await {
            thinking += usize::from(phase == Phase::Thinking);
        }
        assert_eq!(
            thinking,
            MAX_ENDING + 1,
            "the current run and the ending ones"
        );
        lock.execute_batch("ROLLBACK").expect("unlocked");
        while thinking < turns {
            let (_, phase) = timeout(Duration::from_secs(5), shell.state())
                .await
                .expect("the rest are taken once the store answers");
            thinking += usize::from(phase == Phase::Thinking);
        }
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close();
}

#[tokio::test]
async fn engine_events_wait_while_the_shell_is_behind_on_data() {
    let scratch = Scratch::new("behind");
    let fill = "0 fill 240000\n0 fill 240000\n0 fill 240000\n0 fill 240000";
    let mut brain = test_support::brain(&scratch, fill);
    let ((control, data), mut shell) = connect("behind").await;
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("one").await;
        let (first, _) = shell.state().await;
        assert_eq!(shell.state().await, (first, Phase::Answering));
        assert_eq!(shell.state().await, (first, Phase::Done));
        // The first run's text is still queued, so the second run's engine waits and the
        // unread data stalls it instead of finishing it.
        shell.submit("two").await;
        let (second, _) = shell.state().await;
        assert_eq!(shell.state().await, (second, Phase::Answering));
        assert_eq!(shell.state().await, (second, Phase::Interrupted));
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close();
}

#[tokio::test]
async fn a_stale_stop_never_drops_a_finished_runs_data() {
    let scratch = Scratch::new("stale-stop");
    let fill = "0 fill 240000\n0 fill 240000\n0 fill 240000\n0 fill 240000";
    let mut brain = test_support::brain(&scratch, fill);
    let ((control, data), mut shell) = connect("stale-stop").await;
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("go").await;
        let (run, _) = shell.state().await;
        assert_eq!(shell.state().await, (run, Phase::Answering));
        assert_eq!(shell.state().await, (run, Phase::Done));
        // Esc after Done: stale, so the text still queued behind credit stays.
        shell.stop(run).await;
        let mut bytes = 0;
        loop {
            let frame = timeout(Duration::from_secs(2), shell.data())
                .await
                .expect("the run's data ends with its RunEnd");
            match frame {
                DataToShell::AssistantText { delta, .. } => {
                    bytes += delta.len();
                    let credit = DataToBrain::Credit {
                        bytes: u32::try_from(delta.len() + 64).expect("small"),
                    };
                    let frame = frame::encode(&credit, Pipe::Data).expect("encodes");
                    shell.data_out.write_all(&frame).await.expect("sent");
                }
                DataToShell::RunEnd { run: ended } if ended == run => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(bytes, 960_000);
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close();
}

// Real time: paused time would jump ahead whenever the runtime waits on the store's own thread.
#[tokio::test]
async fn a_shell_that_stops_reading_data_ends_the_run_as_stalled() {
    let scratch = Scratch::new("stalled");
    let mut brain = test_support::brain(&scratch, "0 fill 240000\n0 fill 240000\n5000 step late");
    // Small buffers, so the data writer blocks inside a write long before credit runs out.
    let (control, control_client) = tokio::io::duplex(4096);
    let (data, data_client) = tokio::io::duplex(4096);
    let (mut control_in, mut control_out) = tokio::io::split(control_client);
    let (_data_in, mut data_out) = tokio::io::split(data_client);
    let mut tap = Tap::from_env();
    let client = async {
        let submit = DataToBrain::TextSubmit {
            thread: None,
            text: "go".to_owned(),
        };
        data_out
            .write_all(&frame::encode(&submit, Pipe::Data).expect("encodes"))
            .await
            .expect("sent");
        let started = tokio::time::Instant::now();
        let mut phases = Vec::new();
        // Answer every ping, read nothing on data, until the run ends.
        loop {
            match ipc::read::<ControlToShell>(&mut control_in, Pipe::Control)
                .await
                .expect("readable")
                .expect("open")
            {
                ControlToShell::Ping => {
                    let pong =
                        frame::encode(&ControlToBrain::Pong, Pipe::Control).expect("encodes");
                    control_out.write_all(&pong).await.expect("sent");
                }
                ControlToShell::RunState { phase, .. } => {
                    phases.push(phase);
                    if phase == Phase::Interrupted {
                        break;
                    }
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(
            phases,
            [Phase::Thinking, Phase::Answering, Phase::Interrupted]
        );
        let after = started.elapsed();
        assert!(
            (Duration::from_secs(2)..Duration::from_secs(3)).contains(&after),
            "stalled after {after:?}"
        );
        drop((control_in, control_out, data_out));
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close();
    let store = Store::open(&scratch.db()).expect("reopen");
    let reason: String = store
        .conn_for_tests()
        .query_row("SELECT reason FROM runs", [], |row| row.get(0))
        .expect("one run");
    assert_eq!(reason, "view_stalled");
}

/// Rez measurement for the pull request, not a check: run it with
/// `cargo test --release -p capsula-brain -- --ignored --nocapture measure_`.
#[tokio::test]
#[ignore = "a measurement, run by hand on Rez"]
async fn measure_stop_under_a_saturated_writer_and_throughput() {
    let script = "0 fill 240000\n0 fill 240000\n0 fill 240000\n0 fill 240000\n5000 step late";
    let mut stops = Vec::new();
    let mut streams = Vec::new();
    for round in 0..20 {
        let scratch = Scratch::new("measure");
        let mut brain = test_support::brain(&scratch, script);
        let ((control, data), mut shell) = connect(&format!("measure-{round}")).await;
        let mut tap = Tap::from_env();
        let client = async {
            // Throughput: one run read to its last byte, granting credit as a shell would.
            shell.submit("read").await;
            let (run, _) = shell.state().await;
            let mut started = None;
            let mut bytes = 0;
            while bytes < 960_000 {
                let DataToShell::AssistantText { delta, .. } = shell.data().await else {
                    panic!("text only");
                };
                started.get_or_insert_with(tokio::time::Instant::now);
                bytes += delta.len();
                let credit = DataToBrain::Credit {
                    bytes: u32::try_from(delta.len() + 64).expect("small"),
                };
                let frame = frame::encode(&credit, Pipe::Data).expect("encodes");
                shell.data_out.write_all(&frame).await.expect("sent");
            }
            streams.push(started.expect("text arrived").elapsed());
            shell.stop(run).await;
            while shell.state().await != (run, Phase::Interrupted) {}
            // Stop: a second run that nothing reads.
            shell.submit("ignore").await;
            let (run, _) = shell.state().await;
            assert_eq!(shell.state().await, (run, Phase::Answering));
            tokio::time::sleep(Duration::from_millis(100)).await;
            let sent = tokio::time::Instant::now();
            shell.stop(run).await;
            assert_eq!(shell.state().await, (run, Phase::Interrupted));
            stops.push(sent.elapsed());
            drop(shell);
        };
        let (_, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
        brain.close();
    }
    let report = |name: &str, samples: &mut Vec<Duration>| {
        samples.sort_unstable();
        eprintln!(
            "{name}: p50 {:?} p95 {:?} max {:?} (n={})",
            samples[samples.len() / 2],
            samples[samples.len() * 95 / 100],
            samples[samples.len() - 1],
            samples.len()
        );
    };
    report("Stop -> Interrupted, data writer blocked", &mut stops);
    report(
        "960 kB of text from its first frame to its last",
        &mut streams,
    );
}
