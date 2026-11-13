use std::collections::BTreeMap;
use std::time::Duration;

use kumochi_protocol::frame::{self, FrameError, Pipe};
use serde::Serialize;
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::{
    CHUNK, Frame, Reassembly as _, SCRUBBED, Zeroed, ZeroedBody, write_message, write_sized,
};
use crate::supervise::{Destination, Generation, Kind, Pending, WorkerLost, accept};

/// A kind whose replies can carry content, never started.
struct Sealed;

impl Kind for Sealed {
    const ARG: &'static str = "--sealed";
    const NAME: &'static str = "sealed";
    const MAX_REQUEST: usize = 1024;
    const MAX_REPLY: usize = 4 * 1024 * 1024;
    type Request = Vec<u8>;
    type Reply = String;
    type Assembly = Zeroed;
}

fn sized(id: u64, message: &impl Serialize) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_sized(&mut bytes, id, message, Sealed::MAX_REPLY).expect("written");
    bytes
}

fn plain(id: u64, message: &impl Serialize) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_message(&mut bytes, id, message, Sealed::MAX_REPLY).expect("written");
    bytes
}

fn read_all(bytes: &[u8]) -> Vec<Frame<ZeroedBody>> {
    let mut reader = bytes;
    let mut frames = Vec::new();
    while let Some(next) = Zeroed::read(&mut reader).expect("a frame") {
        frames.push(next);
    }
    frames
}

fn chunk(number: u32, body: Vec<u8>) -> Frame<ZeroedBody> {
    Frame {
        id: 1,
        chunk: number,
        last: false,
        body: ZeroedBody(body),
    }
}

fn last(number: u32, body: Vec<u8>) -> Frame<ZeroedBody> {
    Frame {
        last: true,
        ..chunk(number, body)
    }
}

type Reply = oneshot::Receiver<Result<(String, Generation), WorkerLost>>;

fn pending(ids: [u64; 2]) -> (BTreeMap<u64, Pending<Sealed>>, [Reply; 2]) {
    let mut requests = BTreeMap::new();
    let replies = ids.map(|id| {
        let (answer, receive) = oneshot::channel();
        requests.insert(
            id,
            Pending::new(
                Instant::now() + Duration::from_secs(1),
                Destination::Single(answer),
            ),
        );
        receive
    });
    (requests, replies)
}

#[test]
fn the_sized_writer_writes_the_plain_writers_bytes() {
    for len in [
        0,
        1,
        CHUNK - 8,
        CHUNK,
        2 * CHUNK + 10,
        Sealed::MAX_REPLY - 8,
    ] {
        let text = "x".repeat(len);
        assert!(sized(9, &text) == plain(9, &text), "{len}");
    }
}

#[test]
fn the_sized_writer_refuses_a_message_over_the_cap_and_writes_nothing() {
    let mut bytes = Vec::new();
    let over = "x".repeat(Sealed::MAX_REPLY);
    assert!(matches!(
        write_sized(&mut bytes, 1, &over, Sealed::MAX_REPLY),
        Err(FrameError::TooLarge { .. })
    ));
    assert!(bytes.is_empty());
}

#[test]
fn zeroed_reading_gives_the_plain_reads_frames() {
    let text = "y".repeat(2 * CHUNK + 10);
    let bytes = sized(4, &text);
    let mut reader = bytes.as_slice();
    for read in read_all(&bytes) {
        let plain = frame::read::<Frame>(&mut reader, Pipe::Data)
            .expect("read")
            .expect("a frame");
        assert_eq!(
            (read.id, read.chunk, read.last, read.body.as_ref()),
            (plain.id, plain.chunk, plain.last, plain.body.as_slice())
        );
        assert_eq!(read.body.0.capacity(), plain.body.len(), "read exactly");
    }
}

#[test]
fn a_reply_at_the_cap_fills_one_buffer_and_one_byte_over_is_lost() {
    let max = Sealed::MAX_REPLY;
    let sizes = [vec![CHUNK; max / CHUNK], vec![max % CHUNK]].concat();
    for extra in [0, 1] {
        let mut zeroed = Zeroed::new(max);
        let final_chunk = sizes.len() - 1;
        for (index, size) in sizes.iter().enumerate() {
            let number = u32::try_from(index).expect("17 chunks");
            let frame = if index == final_chunk {
                last(number, vec![7; size + extra])
            } else {
                chunk(number, vec![7; *size])
            };
            let pushed = zeroed.push(frame, <[u8]>::len);
            if index == final_chunk {
                let expected = if extra == 0 {
                    Ok(Some(max))
                } else {
                    Err(WorkerLost::Lost)
                };
                assert_eq!(pushed, expected);
            } else {
                assert_eq!(pushed, Ok(None));
                let held = zeroed.bytes.as_ref().expect("held");
                assert_eq!(held.capacity(), max, "allocated once, at the cap");
            }
        }
    }
}

#[test]
fn a_single_chunk_reply_needs_no_buffer() {
    let mut zeroed = Zeroed::new(Sealed::MAX_REPLY);
    assert_eq!(
        zeroed.push(last(0, vec![1, 2, 3]), <[u8]>::to_vec),
        Ok(Some(vec![1, 2, 3]))
    );
    assert!(zeroed.bytes.is_none());
    assert_eq!(
        zeroed.push(last(0, vec![4]), <[u8]>::to_vec),
        Ok(Some(vec![4]))
    );
}

#[test]
fn zeroed_reassembly_refuses_a_gap_a_repeat_and_an_empty_chunk() {
    let mut zeroed = Zeroed::new(8);
    assert_eq!(zeroed.push(chunk(0, vec![1]), <[u8]>::to_vec), Ok(None));
    assert_eq!(
        zeroed.push(last(2, vec![2]), <[u8]>::to_vec),
        Err(WorkerLost::Lost),
        "a gap"
    );
    let mut zeroed = Zeroed::new(8);
    assert_eq!(zeroed.push(chunk(0, vec![1]), <[u8]>::to_vec), Ok(None));
    assert_eq!(
        zeroed.push(last(0, vec![2]), <[u8]>::to_vec),
        Err(WorkerLost::Lost),
        "a repeat"
    );
    let mut zeroed = Zeroed::new(8);
    assert_eq!(
        zeroed.push(last(0, vec![]), <[u8]>::to_vec),
        Err(WorkerLost::Lost),
        "empty"
    );
    let mut zeroed = Zeroed::new(8);
    assert_eq!(zeroed.push(chunk(0, vec![1]), <[u8]>::to_vec), Ok(None));
    assert_eq!(
        zeroed.push(last(1, vec![2]), <[u8]>::to_vec),
        Ok(Some(vec![1, 2]))
    );
}

#[test]
fn every_zeroed_frame_is_zeroed_when_dropped_pushed_or_not() {
    SCRUBBED.take();
    let mut zeroed = Zeroed::new(8);
    assert_eq!(
        zeroed.push(last(0, vec![7; 3]), <[u8]>::to_vec),
        Ok(Some(vec![7; 3]))
    );
    let (mut requests, _replies) = pending([1, 2]);
    let stray = Frame {
        id: 3,
        ..last(0, vec![7; 5])
    };
    assert_eq!(
        accept(&mut requests, stray, Generation(0)),
        Err(WorkerLost::Lost),
        "for no request"
    );
    let (frames, reads) = tokio::sync::mpsc::channel(1);
    frames.try_send(Some(last(0, vec![7; 6]))).expect("room");
    drop((frames, reads));
    assert_eq!(SCRUBBED.take(), [(true, 3), (true, 5), (true, 6)]);
}

#[test]
fn two_interleaved_replies_each_reassemble() {
    let (first, second) = ("a".repeat(CHUNK + 5), "b".repeat(CHUNK + 9));
    let mut ones = read_all(&sized(1, &first));
    let mut twos = read_all(&sized(2, &second));
    assert_eq!((ones.len(), twos.len()), (2, 2));
    let (mut requests, [mut one, mut two]) = pending([1, 2]);
    for frame in [
        ones.remove(0),
        twos.remove(0),
        ones.remove(0),
        twos.remove(0),
    ] {
        assert_eq!(accept(&mut requests, frame, Generation(0)), Ok(()));
    }
    assert_eq!(one.try_recv(), Ok(Ok((first, Generation(0)))));
    assert_eq!(two.try_recv(), Ok(Ok((second, Generation(0)))));
}

#[test]
fn a_truncated_stream_is_an_error_and_a_closed_one_is_none() {
    let bytes = sized(1, &"z".repeat(10));
    assert!(matches!(Zeroed::read(&mut [].as_slice()), Ok(None)));
    for cut in [2, bytes.len() - 1] {
        assert!(
            matches!(Zeroed::read(&mut &bytes[..cut]), Err(FrameError::Truncated)),
            "{cut}"
        );
    }
    let over = u32::try_from(Pipe::Data.max_payload() + 1)
        .expect("small")
        .to_le_bytes();
    assert!(matches!(
        Zeroed::read(&mut over.as_slice()),
        Err(FrameError::TooLarge { .. })
    ));
}
