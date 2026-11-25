use std::cell::Cell;
use std::future::{pending, poll_fn};
use std::io;
use std::pin::{Pin, pin};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    AccountOp, AccountView, AudioSession, ControlToBrain, ControlToShell, DATA_WINDOW, DataToBrain,
    DataToShell, MicPcm, Narration, OutputGeneration, PageOf, Phase, RunId, ScreenChoice, ThreadId,
};
use tokio::io::{
    AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, DuplexStream, ReadBuf,
};
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::{Instant, timeout};

use super::data::{DataQueue, Owner};
use super::terminal::{Fatal, Fence, Terminal};
use super::{Ended, QUIT_GRACE, Seen, close_wait, hold, read_control, read_data};
use crate::coordinator::StoreOp;
use crate::ipc;
use crate::platform::shell_process::{Unconfirmed, Verdict};
use crate::tap::Tap;
use crate::test_support::{self, Scratch};

const AUDIO: [AudioSession; 2] = [AudioSession::Legacy, AudioSession::QueuedRequired];
const LIMIT: Duration = Duration::from_secs(5);
const FIRST: ThreadId = ThreadId(2);
const LATER: ThreadId = ThreadId(3);

fn cells() -> (watch::Sender<bool>, watch::Sender<Verdict>, Terminal) {
    let (pair, refused) = watch::channel(false);
    let (parent, loss) = watch::channel(Verdict::Watching);
    (pair, parent, Terminal::new(refused, Some(loss)))
}

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

fn delete(thread: ThreadId) -> ControlToBrain {
    ControlToBrain::DeleteThread {
        thread,
        after_turns: 0,
        resend: false,
    }
}

fn control(message: &ControlToBrain) -> Vec<u8> {
    frame::encode(message, Pipe::Control).expect("fixture control")
}

fn data(message: &DataToBrain) -> Vec<u8> {
    frame::encode(message, Pipe::Data).expect("fixture data")
}

fn deleted(seen: &mut mpsc::UnboundedReceiver<Seen>) -> Vec<ThreadId> {
    std::iter::from_fn(|| seen.try_recv().ok())
        .filter_map(|event| match event {
            Seen::StoreOpQueued(StoreOp::Delete { thread }) => Some(thread),
            _ => None,
        })
        .collect()
}

#[test]
fn the_first_cause_survives_a_later_parent_loss() {
    let (pair, parent, terminal) = cells();
    let fence = Fence::new(Some(terminal));
    pair.send_replace(true);
    assert_eq!(fence.check(), Some(Fatal::Refused));
    parent.send_replace(Verdict::Exited);
    assert_eq!(fence.check(), Some(Fatal::Refused));
}

#[test]
fn a_pinned_cause_refuses_every_quit_attempt() {
    let (_pair, parent, terminal) = cells();
    let fence = Fence::new(Some(terminal));
    parent.send_replace(Verdict::Unconfirmed(Unconfirmed::Watch));
    assert_eq!(fence.check(), Some(Fatal::ParentUnconfirmed));
    for _ in 0..2 {
        assert_eq!(fence.accept_quit(), Err(Fatal::ParentUnconfirmed));
    }
}

#[test]
fn an_accepted_quit_ignores_later_causes_and_never_wakes_as_fatal() {
    let (pair, parent, terminal) = cells();
    let mut wake = pin!(terminal.clone().wake_on_fatal());
    let fence = Fence::new(Some(terminal));
    assert_eq!(fence.accept_quit(), Ok(()));
    pair.send_replace(true);
    parent.send_replace(Verdict::Exited);
    assert_eq!(fence.check(), None);
    assert_eq!(
        poll_once(pin!(poll_fn(|cx| fence.poll_fatal(wake.as_mut(), cx)))),
        Poll::Pending
    );
}

#[tokio::test]
async fn a_stored_cause_beats_ready_input_while_the_idle_wake_is_pending() {
    let (_pair, parent, terminal) = cells();
    let fence = Fence::new(Some(terminal));
    let mut wake = pin!(pending());
    let (input, mut ready) = mpsc::channel(1);
    let mut acceptance = pin!(async {
        tokio::select! {
            biased;
            cause = poll_fn(|cx| fence.poll_fatal(wake.as_mut(), cx)) => Err(cause),
            value = ready.recv() => Ok(value),
        }
    });
    assert_eq!(poll_once(acceptance.as_mut()), Poll::Pending);
    parent.send_replace(Verdict::Exited);
    input.try_send(FIRST).unwrap();
    assert_eq!(
        poll_once(acceptance.as_mut()),
        Poll::Ready(Err(Fatal::ParentExited))
    );
}

#[test]
fn a_closed_watch_is_terminal_even_without_a_published_cause() {
    let (pair, _parent, terminal) = cells();
    let fence = Fence::new(Some(terminal));
    drop(pair);
    assert_eq!(fence.check(), Some(Fatal::Refused));
    let (_pair, parent, terminal) = cells();
    let fence = Fence::new(Some(terminal));
    drop(parent);
    assert_eq!(fence.check(), Some(Fatal::ParentUnconfirmed));
}

#[test]
fn initial_values_are_checked_even_when_the_receivers_have_seen_them() {
    let (_pair, pair) = watch::channel(true);
    let (_parent, parent) = watch::channel(Verdict::Exited);
    let fence = Fence::new(Some(Terminal::new(pair, Some(parent))));
    assert_eq!(fence.check(), Some(Fatal::ParentExited));
    let (_pair, pair) = watch::channel(true);
    let fence = Fence::new(Some(Terminal::new(pair, None)));
    assert_eq!(fence.check(), Some(Fatal::Refused));
}

#[tokio::test]
async fn fatal_refuses_quit_on_first_and_resumed_reads() {
    for resumed in [false, true] {
        let (pair, _parent, terminal) = cells();
        let fence = Fence::new(Some(terminal));
        let (frames, mut accepted) = mpsc::channel(1);
        let (mut input, mut shell) = tokio::io::duplex(256);
        let quit = Cell::new(None);
        let mut reader = pin!(read_control(&mut input, &frames, &quit, &fence));
        if resumed {
            assert_eq!(poll_once(reader.as_mut()), Poll::Pending);
        }
        pair.send_replace(true);
        shell
            .write_all(&control(&ControlToBrain::Quitting))
            .await
            .unwrap();
        assert_eq!(reader.await, Ended::Fatal(Fatal::Refused));
        assert_eq!(quit.get(), None);
        shell.write_all(&control(&delete(FIRST))).await.unwrap();
        assert_eq!(accepted.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    }
}

#[tokio::test]
async fn fatal_refuses_delete_with_room_or_after_waiting_for_room() {
    for full in [false, true] {
        let (pair, _parent, terminal) = cells();
        let fence = Fence::new(Some(terminal));
        let (frames, mut accepted) = mpsc::channel(1);
        let quit = Cell::new(None);
        let (mut input, mut shell) = tokio::io::duplex(256);
        if full {
            frames.try_send((ControlToBrain::Pong, 0)).unwrap();
        }
        let mut reader = pin!(read_control(&mut input, &frames, &quit, &fence));
        if !full {
            pair.send_replace(true);
        }
        shell.write_all(&control(&delete(FIRST))).await.unwrap();
        if full {
            assert_eq!(poll_once(reader.as_mut()), Poll::Pending);
            pair.send_replace(true);
            assert_eq!(accepted.try_recv(), Ok((ControlToBrain::Pong, 0)));
        }
        shell.shutdown().await.unwrap();
        assert_eq!(reader.await, Ended::Fatal(Fatal::Refused));
        assert_eq!(accepted.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        assert_eq!(quit.get(), None);
    }
}

#[tokio::test(start_paused = true)]
async fn a_quit_latches_without_waiting_for_room_and_keeps_only_its_prefix() {
    let (pair, parent, terminal) = cells();
    let fence = Fence::new(Some(terminal));
    let (frames, mut accepted) = mpsc::channel(1);
    let quit = Cell::new(None);
    let first = control(&delete(FIRST));
    let frame = [
        first.clone(),
        control(&ControlToBrain::Quitting),
        control(&delete(LATER)),
    ]
    .concat();
    let mut input = frame.as_slice();
    let deadline = Instant::now() + close_wait();
    assert_eq!(
        timeout(LIMIT, read_control(&mut input, &frames, &quit, &fence))
            .await
            .unwrap(),
        Ended::Quit { deadline }
    );
    assert_eq!(quit.get(), Some(deadline));
    assert_eq!(accepted.try_recv().unwrap().0, delete(FIRST));
    assert_eq!(accepted.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    assert_eq!(input, control(&delete(LATER)));
    pair.send_replace(true);
    parent.send_replace(Verdict::Exited);
    assert_eq!(fence.check(), None);
}

fn exhausted_queue() -> std::cell::RefCell<DataQueue> {
    let mut queue = DataQueue::new(10);
    queue.push(Owner::Run(RunId(1)), vec![1; 10]);
    assert_eq!(queue.pop(Instant::now().into_std()), Some(vec![1; 10]));
    queue.wrote();
    queue.push(Owner::Run(RunId(1)), vec![2; 5]);
    std::cell::RefCell::new(queue)
}

#[tokio::test]
async fn fatal_rejects_pcm_and_credit_on_first_and_resumed_reads() {
    let scratch = Scratch::new("fatal-data-reader");
    let mut brain = test_support::brain(&scratch, "");
    hold::tests::recording(&mut brain);
    let pcm = MicPcm {
        gesture: hold::tests::KEY.gesture,
        rate: 16_000,
        seq: 0,
        last: false,
        samples: vec![19; 320],
    };
    for resumed in [false, true] {
        for message in [
            DataToBrain::MicPcm(pcm.clone()),
            DataToBrain::Credit { bytes: 5 },
        ] {
            let (pair, _parent, terminal) = cells();
            let fence = Fence::new(Some(terminal));
            let queue = exhausted_queue();
            let ready = Notify::new();
            let (turns, _turns) = mpsc::channel(1);
            let (pages, _pages) = mpsc::channel(1);
            let ingress = brain.holds.ingress();
            let (mut input, mut shell) = tokio::io::duplex(4096);
            let mut reader = pin!(read_data(
                &mut input,
                &turns,
                &pages,
                &queue,
                &ready,
                1,
                Some(&ingress),
                None,
                &fence,
            ));
            if resumed {
                assert_eq!(poll_once(reader.as_mut()), Poll::Pending);
            }
            pair.send_replace(true);
            shell.write_all(&data(&message)).await.unwrap();
            shell.shutdown().await.unwrap();
            assert_eq!(reader.await, Ended::Fatal(Fatal::Refused));
            assert!(poll_once(pin!(hold::next(&mut brain.holds))).is_pending());
            assert_eq!(queue.borrow_mut().pop(Instant::now().into_std()), None);
        }
    }
    brain.close().await;
}

#[tokio::test]
async fn both_readers_prioritize_refusal_at_eof() {
    let (pair, _parent, terminal) = cells();
    let fence = Fence::new(Some(terminal));
    pair.send_replace(true);
    let (frames, _frames) = mpsc::channel(1);
    assert_eq!(
        read_control(&mut tokio::io::empty(), &frames, &Cell::new(None), &fence).await,
        Ended::Fatal(Fatal::Refused)
    );
    let (turns, _turns) = mpsc::channel(1);
    let (pages, _pages) = mpsc::channel(1);
    assert_eq!(
        read_data(
            &mut tokio::io::empty(),
            &turns,
            &pages,
            &exhausted_queue(),
            &Notify::new(),
            1,
            None,
            None,
            &fence,
        )
        .await,
        Ended::Fatal(Fatal::Refused)
    );
}

#[tokio::test]
async fn an_initial_cause_refuses_to_open_the_session() {
    for audio in AUDIO {
        let scratch = Scratch::new("fatal-before-serve");
        let mut brain = test_support::brain(&scratch, "");
        let epoch = brain.epoch;
        let mut tap = Tap::from_env();
        let (control_pipe, mut shell_control) = tokio::io::duplex(8192);
        let (data_pipe, mut shell_data) = tokio::io::duplex(8192);
        let (pair, _parent, terminal) = cells();
        pair.send_replace(true);
        shell_control
            .write_all(&control(&ControlToBrain::Quitting))
            .await
            .unwrap();
        shell_data
            .write_all(&data(&DataToBrain::Credit { bytes: 5 }))
            .await
            .unwrap();
        let ended = timeout(
            LIMIT,
            brain.serve_with_profile(
                control_pipe,
                data_pipe,
                &mut tap,
                audio,
                None,
                Some(terminal),
            ),
        )
        .await
        .unwrap();
        assert_eq!(ended, Ended::Fatal(Fatal::Refused));
        assert_eq!(brain.epoch, epoch);
        for mut reader in [shell_control, shell_data] {
            let mut bytes = Vec::new();
            assert_eq!(reader.read_to_end(&mut bytes).await.unwrap(), 0);
        }
        brain.close().await;
    }
}

#[tokio::test(start_paused = true)]
async fn an_idle_core_wakes_for_parent_exit_without_new_input() {
    for audio in AUDIO {
        let scratch = Scratch::new("fatal-idle");
        let mut brain = test_support::brain(&scratch, "");
        let mut tap = Tap::from_env();
        let (control_pipe, _shell_control) = tokio::io::duplex(8192);
        let (data_pipe, _shell_data) = tokio::io::duplex(8192);
        let (_pair, parent, terminal) = cells();
        {
            let mut serve = pin!(brain.serve_with_profile(
                control_pipe,
                data_pipe,
                &mut tap,
                audio,
                None,
                Some(terminal),
            ));
            assert_eq!(poll_once(serve.as_mut()), Poll::Pending);
            let _publish = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(1)).await;
                parent.send_replace(Verdict::Exited);
            });
            let deadline = Instant::now() + QUIT_GRACE / 2;
            let ended = tokio::time::timeout_at(deadline, serve)
                .await
                .expect("the parent wakes the idle core");
            assert_eq!(ended, Ended::Fatal(Fatal::ParentExited));
            assert!(Instant::now() < deadline);
        }
        brain.close().await;
    }
}

#[tokio::test]
async fn a_cause_stored_next_to_inputs_refuses_them_before_dispatch() {
    for audio in AUDIO {
        let scratch = Scratch::new("fatal-before-input");
        let mut brain =
            test_support::brain(&scratch, "0 fill 240000\n0 fill 240000\n0 fill 240000");
        let mut seen = brain.observe();
        let mut tap = Tap::from_env();
        let (control_pipe, mut shell_control) = tokio::io::duplex(8192);
        let (data_pipe, mut shell_data) = tokio::io::duplex(8192);
        let (pair, _parent, terminal) = cells();
        {
            let mut serve = pin!(brain.serve_with_profile(
                control_pipe,
                data_pipe,
                &mut tap,
                audio,
                None,
                Some(terminal),
            ));
            tokio::select! {
                ended = &mut serve => panic!("connection ended before credit was spent: {ended:?}"),
                ready = timeout(LIMIT, spend_credit(&mut shell_control, &mut shell_data, audio)) => {
                    ready.expect("one reply is waiting for credit");
                }
            }
            pair.send_replace(true);
            shell_control
                .write_all(&[control(&delete(FIRST)), control(&ControlToBrain::Quitting)].concat())
                .await
                .unwrap();
            shell_data
                .write_all(&data(&DataToBrain::Credit { bytes: DATA_WINDOW }))
                .await
                .unwrap();
            assert_eq!(
                timeout(LIMIT, serve).await.unwrap(),
                Ended::Fatal(Fatal::Refused)
            );
        }
        assert_eq!(deleted(&mut seen), []);
        assert_eq!(shell_data.read_to_end(&mut Vec::new()).await.unwrap(), 0);
        assert_eq!(
            shell_control
                .write_all(&control(&delete(LATER)))
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        brain.close().await;
    }
}

async fn spend_credit(
    shell_control: &mut DuplexStream,
    shell_data: &mut DuplexStream,
    audio: AudioSession,
) {
    loop {
        let frame = ipc::read::<ControlToShell>(shell_control, Pipe::Control)
            .await
            .unwrap()
            .expect("open control leg");
        if matches!(frame, ControlToShell::Scenes { .. }) {
            break;
        }
    }
    if audio == AudioSession::QueuedRequired {
        let output = OutputGeneration(1);
        shell_control
            .write_all(&control(&ControlToBrain::QueuedAudioOutput(
                24_000,
                output,
                Narration::Plain,
            )))
            .await
            .unwrap();
        loop {
            let frame = ipc::read::<ControlToShell>(shell_control, Pipe::Control)
                .await
                .unwrap()
                .expect("open control leg");
            if frame == ControlToShell::QueuedAudioRegistered(output) {
                break;
            }
        }
    }
    shell_data
        .write_all(&data(&DataToBrain::TextSubmit {
            turn: 1,
            thread: None,
            screen: ScreenChoice::None,
            text: "go".into(),
        }))
        .await
        .unwrap();
    let mut text_bytes = 0;
    while text_bytes < 480_000 {
        match ipc::read::<DataToShell>(shell_data, Pipe::Data)
            .await
            .unwrap()
        {
            Some(DataToShell::AssistantText { delta, .. }) => text_bytes += delta.len(),
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
    assert_eq!(text_bytes, 480_000);
    loop {
        let frame = ipc::read::<ControlToShell>(shell_control, Pipe::Control)
            .await
            .unwrap()
            .expect("open control leg");
        if matches!(
            frame,
            ControlToShell::RunState {
                run: RunId(1),
                phase: Phase::Done,
                ..
            }
        ) {
            break;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn an_accepted_quit_drains_its_delete_but_not_later_controls() {
    for audio in AUDIO {
        let scratch = Scratch::new("fatal-quit-prefix");
        let mut brain = test_support::brain(&scratch, "");
        let mut seen = brain.observe();
        let mut tap = Tap::from_env();
        let (control_pipe, mut shell_control) = tokio::io::duplex(8192);
        let (data_pipe, _shell_data) = tokio::io::duplex(8192);
        let (_pair, _parent, terminal) = cells();
        shell_control
            .write_all(
                &[
                    control(&delete(FIRST)),
                    control(&ControlToBrain::Quitting),
                    control(&delete(LATER)),
                ]
                .concat(),
            )
            .await
            .unwrap();
        let deadline = Instant::now() + close_wait();
        let ended = timeout(
            LIMIT,
            brain.serve_with_profile(
                control_pipe,
                data_pipe,
                &mut tap,
                audio,
                None,
                Some(terminal),
            ),
        )
        .await
        .unwrap();
        assert_eq!(ended, Ended::Quit { deadline });
        assert_eq!(deleted(&mut seen), [FIRST]);
        brain.close().await;
    }
}

fn assert_grace(seen: &mut mpsc::UnboundedReceiver<Seen>) {
    assert!(
        std::iter::from_fn(|| seen.try_recv().ok()).any(|event| event == Seen::Grace),
        "the data leg has ended and the control reader is in grace",
    );
}

#[tokio::test(start_paused = true)]
async fn fatal_interrupts_grace_with_an_empty_control_pipe() {
    for audio in AUDIO {
        let scratch = Scratch::new("fatal-during-grace");
        let mut brain = test_support::brain(&scratch, "");
        let mut seen = brain.observe();
        let mut tap = Tap::from_env();
        let (control_pipe, _shell_control) = tokio::io::duplex(8192);
        let (data_pipe, shell_data) = tokio::io::duplex(8192);
        let (_pair, parent, terminal) = cells();
        {
            let mut serve = pin!(brain.serve_with_profile(
                control_pipe,
                data_pipe,
                &mut tap,
                audio,
                None,
                Some(terminal),
            ));
            assert_eq!(poll_once(serve.as_mut()), Poll::Pending);
            drop(shell_data);
            assert_eq!(poll_once(serve.as_mut()), Poll::Pending);
            assert_grace(&mut seen);
            let _publish = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(1)).await;
                parent.send_replace(Verdict::Exited);
            });
            let deadline = Instant::now() + QUIT_GRACE / 2;
            let ended = tokio::time::timeout_at(deadline, serve)
                .await
                .expect("fatal ends grace before its deadline");
            assert_eq!(ended, Ended::Fatal(Fatal::ParentExited));
            assert!(Instant::now() < deadline);
        }
        brain.close().await;
    }
}

#[tokio::test]
async fn loss_without_a_cause_still_applies_an_account_op_during_grace() {
    for audio in AUDIO {
        let scratch = Scratch::new("fatal-grace-account");
        let mut brain = test_support::brain(&scratch, "");
        let mut seen = brain.observe();
        let mut tap = Tap::from_env();
        let (control_pipe, mut shell_control) = tokio::io::duplex(8192);
        let (data_pipe, shell_data) = tokio::io::duplex(8192);
        let (_pair, _parent, terminal) = cells();
        {
            let mut serve = pin!(brain.serve_with_profile(
                control_pipe,
                data_pipe,
                &mut tap,
                audio,
                None,
                Some(terminal),
            ));
            assert_eq!(poll_once(serve.as_mut()), Poll::Pending);
            drop(shell_data);
            assert_eq!(poll_once(serve.as_mut()), Poll::Pending);
            assert_grace(&mut seen);
            shell_control
                .write_all(&control(&ControlToBrain::AccountOp(AccountOp::SignIn)))
                .await
                .unwrap();
            assert_eq!(poll_once(serve.as_mut()), Poll::Pending);
            shell_control.shutdown().await.unwrap();
            assert_eq!(timeout(LIMIT, serve).await.unwrap(), Ended::Lost);
        }
        assert_eq!(brain.account.view(), AccountView::SigningIn);
        brain.close().await;
    }
}

struct RefusingPipe {
    stream: DuplexStream,
    refuse_read: bool,
    pair: watch::Sender<bool>,
}

impl AsyncRead for RefusingPipe {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.refuse_read {
            let mut byte = [0];
            let mut trigger = ReadBuf::new(&mut byte);
            std::task::ready!(Pin::new(&mut self.stream).poll_read(cx, &mut trigger))?;
            self.pair.send_replace(true);
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for RefusingPipe {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if !self.refuse_read {
            self.pair.send_replace(true);
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        Pin::new(&mut self.stream).poll_write(cx, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[tokio::test]
async fn a_read_that_refuses_during_grace_cannot_end_as_loss() {
    for audio in AUDIO {
        let scratch = Scratch::new("fatal-grace-read");
        let mut brain = test_support::brain(&scratch, "");
        let mut seen = brain.observe();
        let mut tap = Tap::from_env();
        let (stream, mut shell_control) = tokio::io::duplex(8192);
        let (data_pipe, shell_data) = tokio::io::duplex(8192);
        let (pair, _parent, terminal) = cells();
        let control_pipe = RefusingPipe {
            stream,
            refuse_read: true,
            pair,
        };
        {
            let mut serve = pin!(brain.serve_with_profile(
                control_pipe,
                data_pipe,
                &mut tap,
                audio,
                None,
                Some(terminal),
            ));
            assert_eq!(poll_once(serve.as_mut()), Poll::Pending);
            drop(shell_data);
            assert_eq!(poll_once(serve.as_mut()), Poll::Pending);
            assert_grace(&mut seen);
            shell_control
                .write_all(&control(&ControlToBrain::Pong))
                .await
                .unwrap();
            assert_eq!(
                timeout(LIMIT, serve).await.unwrap(),
                Ended::Fatal(Fatal::Refused)
            );
        }
        assert_eq!(deleted(&mut seen), []);
        brain.close().await;
    }
}

#[tokio::test]
async fn a_writer_refusal_skips_grace_even_though_the_reader_is_still_open() {
    for audio in AUDIO {
        let scratch = Scratch::new("fatal-data-writer");
        let mut brain = test_support::brain(&scratch, "");
        let mut seen = brain.observe();
        let mut tap = Tap::from_env();
        let (control_pipe, _shell_control) = tokio::io::duplex(8192);
        let (stream, mut shell_data) = tokio::io::duplex(8192);
        let (pair, _parent, terminal) = cells();
        let data_pipe = RefusingPipe {
            stream,
            refuse_read: false,
            pair,
        };
        shell_data
            .write_all(&data(&DataToBrain::PageRequest {
                request: 1,
                page: PageOf::Threads {
                    search: None,
                    after: None,
                },
            }))
            .await
            .unwrap();
        assert_eq!(
            timeout(
                LIMIT,
                brain.serve_with_profile(
                    control_pipe,
                    data_pipe,
                    &mut tap,
                    audio,
                    None,
                    Some(terminal),
                )
            )
            .await
            .unwrap(),
            Ended::Fatal(Fatal::Refused)
        );
        assert!(
            std::iter::from_fn(|| seen.try_recv().ok()).all(|event| event != Seen::Grace),
            "a refusal from the data writer doesn't open control grace",
        );
        brain.close().await;
    }
}
