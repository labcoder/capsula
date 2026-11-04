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
