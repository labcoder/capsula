use super::*;

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "fixture mirrors the wire last flag"
)]
fn chunk(number: u32, last: bool, bytes: Vec<u8>) -> Reply {
    Reply {
        id: 1,
        chunk: number,
        last,
        body: bytes,
    }
}

#[test]
fn frame_cap_and_malformed_inputs() {
    let at_cap = Reply {
        id: 1,
        chunk: 0,
        last: false,
        body: vec![42; Pipe::Data.max_payload() - 6],
    };
    let encoded = frame::encode(&at_cap, Pipe::Data).expect("exact frame cap");
    assert_eq!(encoded.len(), 256 * 1024 + 4);
    let decoded = frame::read::<Reply>(&mut encoded.as_slice(), Pipe::Data)
        .expect("read")
        .expect("reply");
    assert_eq!(decoded.body, vec![42; 256 * 1024 - 6]);
    let over = (256 * 1024 + 1_u32).to_le_bytes().to_vec();
    let empty = 0_u32.to_le_bytes().to_vec();
    let mistyped = frame::encode(
        &Request {
            id: 1,
            body: RequestBody::Ping,
        },
        Pipe::Data,
    )
    .expect("request");
    for bytes in [
        over,
        empty,
        encoded[..encoded.len() - 1].to_vec(),
        mistyped,
        vec![255; 4],
    ] {
        assert_eq!(
            frame::read::<Reply>(&mut bytes.as_slice(), Pipe::Data)
                .map(|reply| reply.map(|reply| reply.id))
                .map_err(|_| ScreenError::WorkerLost),
            Err(ScreenError::WorkerLost)
        );
    }
    assert_eq!(
        frame::read::<Reply>(&mut [].as_slice(), Pipe::Data)
            .expect("EOF")
            .map(|r| r.id),
        None
    );
}

#[test]
fn reassembly_exact_cap_one_over_empty_gap_and_duplicate() {
    let mut assembly = Assembly::default();
    for n in 0..16 {
        let reply = assembly
            .push(chunk(n, n == 15, vec![42; 256 * 1024]))
            .expect("bounded chunks");
        if n == 15 {
            assert_eq!(reply, Some(vec![42; 4 * 1024 * 1024]));
        } else {
            assert_eq!(reply, None);
        }
    }
    let mut assembly = Assembly::default();
    assert_eq!(
        assembly.push(chunk(0, false, vec![0; MAX_REPLY_BYTES])),
        Ok(None)
    );
    assert_eq!(
        assembly.push(chunk(1, true, vec![0])),
        Err(ScreenError::WorkerLost)
    );
    for reply in [chunk(0, true, vec![]), chunk(1, true, vec![0])] {
        assert_eq!(
            Assembly::default().push(reply),
            Err(ScreenError::WorkerLost)
        );
    }
    let mut assembly = Assembly::default();
    assert_eq!(assembly.push(chunk(0, false, vec![0])), Ok(None));
    assert_eq!(
        assembly.push(chunk(0, true, vec![0])),
        Err(ScreenError::WorkerLost)
    );
}

#[test]
fn request_debug_is_redacted() {
    assert_eq!(format!("{:?}", RequestBody::Ping), "RequestBody(***)");
}

#[test]
fn capture_replies_round_trip_and_a_17_chunk_reply_reassembles_at_4_mib() {
    use kumochi_protocol::geometry::PhysicalRect;

    let captured = ReplyBody::Captured(FrameMeta {
        capture: CaptureId(u64::MAX),
        display: PhysicalRect {
            origin: [i32::MIN, -1440],
            size: [8192, 8192],
        },
        dpi: 288,
        acquired_after_ms: u32::MAX,
    });
    for body in [captured, ReplyBody::Refused(Refusal::DisplayChanged)] {
        let sent = reply(4, &body).expect("small reply");
        let mut assembly = Assembly::default();
        let bytes = assembly.push(sent).expect("one chunk").expect("complete");
        assert_eq!(frame::decode::<ReplyBody>(&bytes).expect("decodes"), body);
    }
    let most = Pipe::Data.max_payload() - 6;
    let sizes = [vec![most; 16], vec![MAX_REPLY_BYTES - 16 * most]].concat();
    assert_eq!(sizes.len(), 17);
    let mut assembly = Assembly::default();
    for (index, size) in sizes.iter().enumerate() {
        let last = index == 16;
        let sent = Reply {
            id: 1,
            chunk: u32::try_from(index).expect("17 chunks"),
            last,
            body: vec![42; *size],
        };
        let encoded = frame::encode(&sent, Pipe::Data).expect("within the frame cap");
        let read = frame::read::<Reply>(&mut encoded.as_slice(), Pipe::Data)
            .expect("read")
            .expect("reply");
        let done = assembly.push(read).expect("within the reply cap");
        assert_eq!(
            done.map(|bytes| bytes.len()),
            last.then_some(MAX_REPLY_BYTES)
        );
    }
}

#[test]
fn each_malformed_frame_is_its_own_frame_error() {
    let over = (256 * 1024 + 1_u32).to_le_bytes().to_vec();
    let at_cap = frame::encode(&chunk(0, true, vec![1; 64]), Pipe::Data).expect("frame");
    let truncated = at_cap[..at_cap.len() - 1].to_vec();
    let mistyped = frame::encode(
        &Request {
            id: 1,
            body: RequestBody::Ping,
        },
        Pipe::Data,
    )
    .expect("request");
    let read = |bytes: Vec<u8>| frame::read::<Reply>(&mut bytes.as_slice(), Pipe::Data);
    assert!(matches!(
        read(over),
        Err(frame::FrameError::TooLarge {
            len: 262_145,
            max: 262_144
        })
    ));
    assert!(matches!(read(truncated), Err(frame::FrameError::Truncated)));
    assert!(matches!(
        read(mistyped),
        Err(frame::FrameError::Malformed(_))
    ));
}
