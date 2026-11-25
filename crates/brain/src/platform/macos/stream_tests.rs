use std::cell::Cell;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use kumochi_protocol::frame::{self, HEADER_LEN, Pipe};
use kumochi_protocol::message::{ControlToBrain, ControlToShell, RunId};
use kumochi_session::{
    BrainAwaitsInit, Channel, ENVELOPE_LEN, MAX_PLAINTEXT, MAX_RECORD, Outgoing, Sealer, Session,
    SessionError, ShellAwaitsReply,
};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf, duplex};
use tokio::net::UnixStream;
use tokio::sync::watch;

use super::{RecordReader, RecordWriter, Refusal};
use crate::ipc;

/// A synthetic group secret; no real key is ever used here.
const SECRET: [u8; 32] = [0x5A; 32];

fn envelope(out: &Outgoing) -> [u8; ENVELOPE_LEN] {
    [out.bytes[0], out.bytes[1]]
}

fn body(out: &Outgoing) -> &[u8] {
    &out.bytes[ENVELOPE_LEN..]
}

/// The honest exchange through the session crate's public steps: (shell, brain), each leg's
/// owners already advanced past Finished, Bind and `BindAck`.
fn established() -> (Session, Session) {
    let (mut shell, init) = ShellAwaitsReply::start(&SECRET, Channel::Dev).expect("shell starts");
    let mut brain = BrainAwaitsInit::new(&SECRET, Channel::Dev).expect("brain starts");
    brain.record_len(envelope(&init)).expect("init length");
    let (mut brain, reply) = brain.receive(body(&init)).expect("init");
    shell.record_len(envelope(&reply)).expect("reply length");
    let (shell, [finished, data_init]) = shell.receive(body(&reply)).expect("reply");
    brain
        .record_len(envelope(&finished))
        .expect("Finished length");
    let brain = brain.receive(body(&finished)).expect("Finished");
    brain
        .record_len(envelope(&data_init))
        .expect("data init length");
    let (brain, data_reply) = brain.receive(body(&data_init)).expect("data init");
    shell
        .record_len(envelope(&data_reply))
        .expect("data reply length");
    let (shell, [data_finished, bind]) = shell.receive(body(&data_reply)).expect("data reply");
    brain
        .record_len(envelope(&data_finished))
        .expect("data Finished length");
    let brain = brain.receive(body(&data_finished)).expect("data Finished");
    brain.record_len(envelope(&bind)).expect("Bind length");
    let (brain, _proof, ack) = brain.receive(body(&bind)).expect("Bind");
    shell.record_len(envelope(&ack)).expect("BindAck length");
    let shell = shell.receive(body(&ack)).expect("BindAck");
    (shell, brain)
}

/// A frame of `total` bytes, header included, with a counting payload.
fn frame(total: usize) -> Vec<u8> {
    let payload = total - HEADER_LEN;
    let mut bytes = u32::try_from(payload).expect("u32").to_le_bytes().to_vec();
    bytes.extend((0..payload).map(|i| u8::try_from(i % 251).expect("under 251")));
    bytes
}

/// Seals `plaintext` as maximally sliced records and returns their wire bytes, for a test that
/// writes them to the transport itself.
fn records(sealer: &mut Sealer, plaintext: &[u8]) -> Vec<u8> {
    let mut wire = Vec::new();
    for record in plaintext.chunks(MAX_PLAINTEXT) {
        sealer.seal(record).expect("seals");
        let bytes = sealer.unwritten().expect("sealed").to_vec();
        sealer.advance(bytes.len()).expect("taken");
        wire.extend(bytes);
    }
    wire
}

fn refusal(error: &io::Error) -> Option<Refusal> {
    error.get_ref()?.downcast_ref::<Refusal>().copied()
}

async fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    let mut future = Some(future);
    std::future::poll_fn(|cx| Poll::Ready(future.take().expect("polled once").poll(cx))).await
}

/// The brain's pair cell: the sender its adapters share, and the receiver the runtime reads.
fn cell() -> (watch::Sender<bool>, watch::Receiver<bool>) {
    watch::channel(false)
}

struct InterruptOnce<T>(T, bool);

impl<T: AsyncRead + Unpin> AsyncRead for InterruptOnce<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if std::mem::replace(&mut self.1, false) {
            return Poll::Ready(Err(io::ErrorKind::Interrupted.into()));
        }
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for InterruptOnce<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if std::mem::replace(&mut self.1, false) {
            return Poll::Ready(Err(io::ErrorKind::Interrupted.into()));
        }
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

#[tokio::test]
async fn fragmented_frames_cross_short_reads_and_writes_whole() {
    let (shell, brain) = established();
    let (pair, refused) = cell();
    let (to_brain, at_brain) = duplex(7);
    let mut writer = RecordWriter::new(
        InterruptOnce(to_brain, true),
        shell.data.sealer,
        Pipe::Data,
        cell().0,
    );
    let mut reader = RecordReader::new(InterruptOnce(at_brain, true), brain.data.opener, pair);
    assert_eq!(reader.read(&mut []).await.expect("empty read"), 0);
    let frames = [
        frame(HEADER_LEN + Pipe::Data.max_payload()),
        frame(256 * 1024),
        frame(HEADER_LEN + 1),
        frame(HEADER_LEN),
    ];
    let send = async {
        for bytes in &frames {
            writer.write_frame(bytes).await.expect("sends");
        }
        drop(writer);
    };
    let receive = async {
        let mut got = Vec::new();
        let mut chunk = [0; 3];
        loop {
            let n = reader.read(&mut chunk).await.expect("reads");
            if n == 0 {
                break got;
            }
            got.extend_from_slice(&chunk[..n]);
        }
    };
    let ((), got) = tokio::join!(send, receive);
    assert_eq!(
        got,
        frames.concat(),
        "every frame arrives whole and in order"
    );
    assert!(
        !*refused.borrow(),
        "a clean end between frames is not a refusal"
    );
}

#[tokio::test]
async fn the_stream_may_end_cleanly_only_between_frames() {
    let two_records = frame(HEADER_LEN + MAX_PLAINTEXT);
    let first_record = ENVELOPE_LEN + MAX_PLAINTEXT + 16;
    for (cut, delivered) in [
        (1, 0),
        (7, 0),
        (first_record, MAX_PLAINTEXT),
        (first_record + 1, MAX_PLAINTEXT),
    ] {
        let (mut shell, brain) = established();
        let (pair, refused) = cell();
        let wire = records(&mut shell.data.sealer, &two_records);
        let (mut raw, at_brain) = duplex(wire.len());
        raw.write_all(&wire[..cut]).await.expect("written");
        drop(raw);
        let mut reader = RecordReader::new(at_brain, brain.data.opener, pair);
        let mut got = Vec::new();
        let error = reader.read_to_end(&mut got).await.expect_err("truncated");
        assert_eq!(
            refusal(&error),
            Some(Refusal::Session(SessionError::Truncated)),
            "cut at {cut}"
        );
        assert_eq!(
            got,
            &two_records[..delivered],
            "only whole records, cut at {cut}"
        );
        assert!(*refused.borrow(), "set before the error, cut at {cut}");
    }
}

#[tokio::test]
async fn a_refusal_sets_the_cell_first_and_stops_both_legs() {
    let (mut shell, brain) = established();
    let (pair, refused) = cell();
    let (mut raw_control, at_brain_control) = duplex(1024);
    let (mut raw_data, at_brain_data) = duplex(1024);
    let (to_shell, mut at_shell) = duplex(1024);
    let mut control_in = RecordReader::new(at_brain_control, brain.control.opener, pair.clone());
    let mut data_in = RecordReader::new(at_brain_data, brain.data.opener, pair.clone());
    let mut data_out = RecordWriter::new(to_shell, brain.data.sealer, Pipe::Data, pair);
    let valid = records(&mut shell.data.sealer, &frame(HEADER_LEN + 3));
    raw_data
        .write_all(&valid)
        .await
        .expect("a valid data record waits");
    raw_control.write_all(&[0, 17]).await.expect("envelope");
    raw_control.write_all(&[0; 17]).await.expect("forged body");

    let mut byte = [0; 1];
    let error = control_in.read(&mut byte).await.expect_err("forged");
    assert_eq!(
        refusal(&error),
        Some(Refusal::Session(SessionError::Authentication))
    );
    assert!(*refused.borrow(), "the cell is set when the error arrives");

    let late = data_in.read(&mut byte).await.expect_err("the other leg");
    assert_eq!(
        refusal(&late),
        Some(Refusal::PairRefused),
        "the waiting record isn't opened"
    );
    let late = data_out
        .write_frame(&frame(HEADER_LEN + 1))
        .await
        .expect_err("no write");
    assert_eq!(refusal(&late), Some(Refusal::PairRefused));
    let again = control_in.read(&mut byte).await.expect_err("still refused");
    assert_eq!(
        refusal(&again),
        Some(Refusal::PairRefused),
        "the first refusal stays"
    );
    drop(data_out);
    let mut sent = Vec::new();
    at_shell.read_to_end(&mut sent).await.expect("closed");
    assert!(
        sent.is_empty(),
        "nothing reached the shell after the refusal"
    );
}

#[tokio::test]
async fn bounds_are_refused_one_over_and_taken_at_the_limit() {
    let cap = Pipe::Data.max_payload();
    let at_cap = u32::try_from(cap).expect("u32").to_le_bytes();
    let over = u32::try_from(cap + 1).expect("u32").to_le_bytes();
    for (wire, expected) in [
        (
            None,
            Err(Refusal::Session(SessionError::Length { len: 16 })),
        ),
        (Some(at_cap), Ok(at_cap)),
        (
            Some(over),
            Err(Refusal::Session(SessionError::FrameTooLarge)),
        ),
    ] {
        let (mut shell, brain) = established();
        let (pair, refused) = cell();
        let bytes = match wire {
            Some(header) => records(&mut shell.data.sealer, &header),
            None => vec![0, 16],
        };
        let (mut raw, at_brain) = duplex(64);
        raw.write_all(&bytes).await.expect("written");
        let mut reader = RecordReader::new(at_brain, brain.data.opener, pair);
        let mut header = [0; HEADER_LEN];
        let got = reader
            .read_exact(&mut header)
            .await
            .map(|_| header)
            .map_err(|error| refusal(&error).expect("a refusal"));
        assert_eq!(got, expected);
        assert_eq!(*refused.borrow(), expected.is_err());
    }
}

#[tokio::test]
async fn the_writer_takes_exactly_one_frame_within_the_cap() {
    let mut longer = frame(HEADER_LEN + 4);
    longer[0] = 5;
    let mut over = frame(HEADER_LEN + Pipe::Control.max_payload());
    over.push(0);
    over[..HEADER_LEN].copy_from_slice(
        &u32::try_from(Pipe::Control.max_payload() + 1)
            .expect("u32")
            .to_le_bytes(),
    );
    for bytes in [vec![], vec![1, 0, 0], longer, over] {
        let (_, brain) = established();
        let (pair, refused) = cell();
        let (to_shell, mut at_shell) = duplex(1024);
        let mut writer = RecordWriter::new(to_shell, brain.control.sealer, Pipe::Control, pair);
        let error = writer.write_frame(&bytes).await.expect_err("not one frame");
        assert_eq!(
            refusal(&error),
            Some(Refusal::NotOneFrame),
            "{} bytes",
            bytes.len()
        );
        assert!(*refused.borrow());
        drop(writer);
        let mut sent = Vec::new();
        at_shell.read_to_end(&mut sent).await.expect("closed");
        assert!(sent.is_empty(), "nothing written for {} bytes", bytes.len());
    }
}

/// A transport that always fails as a reset connection.
struct Reset;

impl AsyncRead for Reset {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()))
    }
}

#[tokio::test]
async fn ordinary_transport_loss_is_not_a_refusal() {
    let (_, brain) = established();
    let (pair, refused) = cell();
    let mut reader = RecordReader::new(Reset, brain.control.opener, pair.clone());
    let error = reader.read(&mut [0; 4]).await.expect_err("reset");
    assert_eq!(
        (error.kind(), refusal(&error)),
        (io::ErrorKind::ConnectionReset, None)
    );
    assert!(!*refused.borrow(), "a reset is loss, not refusal");

    let (to_shell, at_shell) = duplex(64);
    drop(at_shell);
    let mut writer = RecordWriter::new(to_shell, brain.data.sealer, Pipe::Data, pair);
    let error = writer
        .write_frame(&frame(HEADER_LEN + 1))
        .await
        .expect_err("closed");
    assert_eq!(
        (error.kind(), refusal(&error)),
        (io::ErrorKind::BrokenPipe, None)
    );
    assert!(!*refused.borrow(), "a broken pipe is loss, not refusal");
    let again = writer
        .write_frame(&frame(HEADER_LEN + 1))
        .await
        .expect_err("no resume");
    assert_eq!(refusal(&again), Some(Refusal::Unfinished));
    assert!(*refused.borrow());
}

#[tokio::test]
async fn dropping_a_pending_frame_refuses_both_legs_immediately() {
    let big = frame(256 * 1024);
    let small = frame(HEADER_LEN + 1);
    for taken in [1_000, ENVELOPE_LEN + MAX_RECORD] {
        let (mut shell, brain) = established();
        let (pair, refused) = cell();
        let (to_shell, mut at_shell) = duplex(taken);
        let (mut raw_control, at_brain_control) = duplex(64);
        let mut writer = RecordWriter::new(to_shell, brain.data.sealer, Pipe::Data, pair.clone());
        let mut control_in = RecordReader::new(at_brain_control, brain.control.opener, pair);
        {
            let write = writer.write_frame(&big);
            tokio::pin!(write);
            assert!(
                poll_once(write.as_mut()).await.is_pending(),
                "{taken}: the transport is full"
            );
        }
        assert!(
            *refused.borrow(),
            "{taken}: dropping an unfinished frame latches refusal"
        );

        let control = frame(HEADER_LEN + 2);
        raw_control
            .write_all(&records(&mut shell.control.sealer, &control))
            .await
            .expect("sent");
        let error = control_in
            .read(&mut [0; HEADER_LEN])
            .await
            .expect_err("the other leg cannot read its ready record");
        assert_eq!(
            refusal(&error),
            Some(Refusal::PairRefused),
            "{taken}: refusal precedes control delivery"
        );

        let again = writer.write_frame(&small).await.expect_err("never resumed");
        assert_eq!(refusal(&again), Some(Refusal::PairRefused), "{taken}");
        drop(writer);
        let mut sent = Vec::new();
        at_shell.read_to_end(&mut sent).await.expect("closed");
        assert_eq!(sent.len(), taken, "{taken}: no byte after the cut");
    }

    let (_, brain) = established();
    let (pair, refused) = cell();
    let (to_shell, mut at_shell) = duplex(64);
    let mut writer = RecordWriter::new(to_shell, brain.data.sealer, Pipe::Data, pair);
    drop(writer.write_frame(&big));
    writer
        .write_frame(&small)
        .await
        .expect("a frame never polled never started");
    assert!(!*refused.borrow());
    drop(writer);
    let mut sent = Vec::new();
    at_shell.read_to_end(&mut sent).await.expect("closed");
    assert_eq!(
        sent.len(),
        ENVELOPE_LEN + small.len() + 16,
        "only the small frame"
    );
}

#[tokio::test]
async fn a_read_cancelled_mid_record_resumes_from_its_offset() {
    let (mut shell, brain) = established();
    let (pair, refused) = cell();
    let sent = frame(HEADER_LEN + 3);
    let wire = records(&mut shell.control.sealer, &sent);
    let (mut raw, at_brain) = duplex(64);
    let mut reader = RecordReader::new(at_brain, brain.control.opener, pair);
    raw.write_all(&wire[..10])
        .await
        .expect("part of the record");
    let mut got = vec![0; sent.len()];
    {
        let read = reader.read(&mut got);
        tokio::pin!(read);
        assert!(
            poll_once(read.as_mut()).await.is_pending(),
            "the record isn't complete"
        );
    }
    raw.write_all(&wire[10..]).await.expect("the rest");
    reader.read_exact(&mut got).await.expect("resumes");
    assert_eq!(got, sent);
    assert!(!*refused.borrow());
}

#[tokio::test]
async fn debug_output_carries_no_plaintext() {
    let (mut shell, brain) = established();
    let (pair, _refused) = cell();
    let mut marker = vec![5, 0, 0, 0];
    marker.extend_from_slice(b"SENTI");
    let wire = records(&mut shell.data.sealer, &marker);
    let (mut raw, at_brain) = duplex(64);
    raw.write_all(&wire).await.expect("sent");
    let mut reader = RecordReader::new(at_brain, brain.data.opener, pair.clone());
    let mut first = [0; 1];
    reader
        .read_exact(&mut first)
        .await
        .expect("one byte; the rest stays buffered");
    let (to_shell, _at_shell) = duplex(64);
    let writer = RecordWriter::new(to_shell, brain.data.sealer, Pipe::Data, pair);
    let text = format!("{reader:?} {writer:?} {:?}", Refusal::Unfinished);
    assert!(!text.contains("SENTI"), "as text: {text}");
    assert!(
        !text.contains("83, 69, 78, 84, 73"),
        "as a byte list: {text}"
    );
}

/// C-7, brain side: the brain's data writer is stuck inside a fragmented 256 KiB frame because
/// the shell stopped reading data, and control still round-trips both ways. Both ends use these
/// tokio adapters; the shell's own blocking adapter is I1e-2's.
#[tokio::test]
async fn control_round_trips_while_data_is_stuck_mid_frame() {
    const FRAMES: usize = 3;
    let (shell, brain) = established();
    let (pair, refused) = cell();
    let (brain_control, shell_control) = UnixStream::pair().expect("control sockets");
    let (brain_data, shell_data) = UnixStream::pair().expect("data sockets");
    let (control_in, control_out) = brain_control.into_split();
    let (_data_in, data_out) = brain_data.into_split();
    let mut control_in = RecordReader::new(control_in, brain.control.opener, pair.clone());
    let mut control_out = RecordWriter::new(
        control_out,
        brain.control.sealer,
        Pipe::Control,
        pair.clone(),
    );
    let mut data_out = RecordWriter::new(data_out, brain.data.sealer, Pipe::Data, pair);
    let (shell_control_in, shell_control_out) = shell_control.into_split();
    let (shell_data_in, _shell_data_out) = shell_data.into_split();
    let (shell_pair, _) = cell();
    let mut shell_control_in =
        RecordReader::new(shell_control_in, shell.control.opener, shell_pair.clone());
    let mut shell_control_out = RecordWriter::new(
        shell_control_out,
        shell.control.sealer,
        Pipe::Control,
        shell_pair.clone(),
    );
    let mut shell_data_in = RecordReader::new(shell_data_in, shell.data.opener, shell_pair);

    let big = frame(256 * 1024);
    let written = Cell::new(0);
    let data = async {
        for _ in 0..FRAMES {
            data_out.write_frame(&big).await.expect("data frame");
            written.set(written.get() + 1);
        }
    };
    tokio::pin!(data);
    let mut received = vec![0; FRAMES * big.len()];
    let (prefix, rest) = received.split_at_mut(big.len() + 2 * MAX_PLAINTEXT + 100);
    tokio::select! {
        biased;
        () = &mut data => panic!("every frame went out before the shell read them"),
        read = shell_data_in.read_exact(prefix) => { read.expect("the shell reads into frame two"); }
    }

    let stop = ControlToBrain::Stop {
        run: RunId(7),
        revealed: vec![0, 1],
    };
    let control = async {
        let ping = frame::encode(&ControlToShell::Ping, Pipe::Control).expect("encodes");
        control_out.write_frame(&ping).await.expect("Ping out");
        let got = ipc::read::<ControlToShell>(&mut shell_control_in, Pipe::Control).await;
        assert_eq!(got.expect("Ping in"), Some(ControlToShell::Ping));
        for message in [ControlToBrain::Pong, stop.clone()] {
            let bytes = frame::encode(&message, Pipe::Control).expect("encodes");
            shell_control_out
                .write_frame(&bytes)
                .await
                .expect("control out");
            let got = ipc::read::<ControlToBrain>(&mut control_in, Pipe::Control).await;
            assert_eq!(got.expect("control in"), Some(message));
        }
    };
    tokio::select! {
        biased;
        () = &mut data => panic!("the data writer finished while the shell's data reader stalled"),
        done = tokio::time::timeout(Duration::from_secs(1), control) => done.expect("control within 1 s"),
    }
    assert_eq!(
        written.get(),
        1,
        "the data writer is still inside frame two"
    );

    let ((), read) = tokio::join!(data, shell_data_in.read_exact(rest));
    read.expect("the shell reads the rest");
    assert_eq!(
        received,
        big.repeat(FRAMES),
        "every frame arrives whole after the stall"
    );
    assert!(!*refused.borrow());
}
