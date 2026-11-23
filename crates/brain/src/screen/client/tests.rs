use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Duration;

use kumochi_protocol::geometry::{PhysicalPoint, PhysicalRect};

use super::*;
use crate::screen::encode::{CaptureTransform, Encoded};
use crate::screen::mask::Completeness;
use crate::screen::wire::FrameMeta;
use crate::supervise::fixture::{self, Rig};

const RECT: PhysicalRect = PhysicalRect {
    origin: [0, 0],
    size: [4, 3],
};

const EXPECTED: TypedDisplay = TypedDisplay {
    id: NonZeroU64::new(15).unwrap(),
    bounds: RECT,
    dpi: 96,
};
const POINT: PhysicalPoint = PhysicalPoint([1, 1]);

fn owner(gesture: u64) -> Owner {
    Owner { epoch: 1, gesture }
}

fn until() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

fn meta(capture: CaptureId) -> FrameMeta {
    FrameMeta {
        capture,
        display: RECT,
        dpi: 96,
        acquired_after_ms: 1,
    }
}

fn held(capture: u64) -> Held {
    let key = Key {
        owner: Some(owner(7)),
        capture: CaptureId(capture),
    };
    Held {
        key,
        generation: Generation(0),
    }
}

fn context() -> ScreenContext {
    ScreenContext::new(Encoded {
        jpeg: zeroize::Zeroizing::new(vec![0xFF, 0xD8, 0xFF, 0xD9]),
        size: [4, 3],
        capture: CaptureId(1),
        generation: 1,
        completeness: Completeness::Complete,
        transform: CaptureTransform {
            display: RECT,
            dpi: 96,
            overview_size: [4, 3],
            crop: None,
        },
    })
}

fn client(rig: &Rig<Holds>) -> Arc<HoldClient> {
    Arc::new(HoldClient::new(rig.handle.clone()))
}

#[tokio::test]
async fn both_capture_paths_share_checked_ids_at_exhaustion() {
    let mut rig = fixture::start::<Holds>();
    let client = client(&rig);
    client.admission.lock().unwrap().last_capture = u64::MAX - 2;
    let hold = tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.context(owner(7), POINT, until()).await }
    });
    let (id, request) = rig.next().await;
    hold_frame(&rig, id, request).await;
    assert_eq!(
        hold.await.unwrap().unwrap().key.capture,
        CaptureId(u64::MAX - 1)
    );

    let typed = tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.typed_display(POINT, EXPECTED, 5, until()).await }
    });
    let (id, request) = rig.next().await;
    hold_frame(&rig, id, request).await;
    let picture = typed.await.unwrap().unwrap();
    assert_eq!(picture.capture_id(), CaptureId(u64::MAX));
    drop(picture);
    let (id, request) = rig.next().await;
    let key = Key {
        owner: None,
        capture: CaptureId(u64::MAX),
    };
    assert!(matches!(request, Request::Drop(dropped) if dropped == key));
    rig.reply(id, &Reply::Revoked(key)).await;

    for error in [
        client.context(owner(8), POINT, until()).await.err(),
        client
            .typed_display(POINT, EXPECTED, 5, until())
            .await
            .err(),
        client.context(owner(9), POINT, until()).await.err(),
    ] {
        assert_eq!(error, Some(Closed::Lost(WorkerLost::Lost)));
    }
}

#[tokio::test]
async fn a_missing_hold_release_receipt_does_not_retire_a_live_typed_capture() {
    let mut rig = fixture::start::<Holds>();
    let client = client(&rig);
    let old = tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.context(owner(7), POINT, until()).await }
    });
    let (id, request) = rig.next().await;
    hold_frame(&rig, id, request).await;
    let generation = old.await.unwrap().unwrap().generation;
    let releasing = tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.retire(owner(7), Some(generation), until()).await }
    });
    let (retire, request) = rig.next().await;
    assert!(matches!(request, Request::Retire(held) if held == owner(7)));
    let typed = tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.typed_display(POINT, EXPECTED, 5, until()).await }
    });
    let (id, request) = rig.next().await;
    rig.reply(retire, &Reply::Busy).await;
    assert_eq!(releasing.await.unwrap(), Err(Unreleased));
    assert_eq!(
        rig.terminations.load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    hold_frame(&rig, id, request).await;
    let picture = typed.await.unwrap().unwrap();
    assert_eq!(picture.capture_id(), CaptureId(2));
}

#[tokio::test]
async fn aborting_typed_acquisition_queues_exact_revocation() {
    let mut rig = fixture::start::<Holds>();
    let client = client(&rig);
    let acquisition = tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.typed_display(POINT, EXPECTED, 5, until()).await }
    });
    let (pending, request) = rig.next().await;
    let Request::TypedDisplay {
        ticket,
        point,
        expected,
        excluded,
    } = request
    else {
        panic!("not typed: {request:?}");
    };
    assert_eq!((point, expected, excluded), (POINT, EXPECTED, 5));
    acquisition.abort();
    assert!(acquisition.await.unwrap_err().is_cancelled());

    let (id, request) = rig.next().await;
    let key = Key {
        owner: None,
        capture: ticket.capture(),
    };
    assert!(matches!(request, Request::Drop(dropped) if dropped == key));
    rig.reply(id, &Reply::Revoked(key)).await;
    rig.reply(pending, &Reply::Refused(Refusal::Stale)).await;
}

#[tokio::test]
async fn typed_seal_rejects_a_replacement_and_cleanup_leaves_it_running() {
    let mut rig = fixture::start_at::<Holds>(Generation(1));
    let key = Key {
        owner: None,
        capture: CaptureId(3),
    };
    let frame = TypedHeld {
        held: Held {
            key,
            generation: Generation(0),
        },
        cleanup: TypedDrop {
            inner: rig.handle.clone(),
            key,
            runtime: tokio::runtime::Handle::current(),
            armed: true,
        },
        until: until(),
    };
    let sealing = tokio::spawn(frame.seal());
    let (id, request) = rig.next().await;
    assert!(matches!(request, Request::Seal(sealed) if sealed == key));
    rig.reply(id, &Reply::Sealed(key, context())).await;
    assert_eq!(
        sealing.await.unwrap().err(),
        Some(Closed::Lost(WorkerLost::Lost))
    );

    let (id, request) = rig.next().await;
    assert!(matches!(request, Request::Drop(dropped) if dropped == key));
    rig.reply(id, &Reply::Refused(Refusal::Stale)).await;
    let inner = rig.handle.clone();
    let ping = tokio::spawn(async move { inner.request(Request::Ping, until()).await });
    let (id, request) = rig.next().await;
    assert!(matches!(request, Request::Ping));
    rig.reply(id, &Reply::Pong).await;
    assert_eq!(ping.await.unwrap(), Ok((Reply::Pong, Generation(1))));
}

#[tokio::test]
async fn each_capture_gets_a_fresh_key_and_a_higher_ticket_and_names_its_worker() {
    let mut rig = fixture::start::<Holds>();
    let client = client(&rig);
    let mut tickets = Vec::new();
    for gesture in [7, 8] {
        let asking = tokio::spawn({
            let client = Arc::clone(&client);
            async move {
                client
                    .context(owner(gesture), PhysicalPoint([5, 6]), until())
                    .await
            }
        });
        let (id, request) = rig.next().await;
        let Request::Context { key, ticket, point } = request else {
            panic!("not a capture: {request:?}")
        };
        assert_eq!(
            (key.owner, ticket.capture(), point),
            (Some(owner(gesture)), key.capture, PhysicalPoint([5, 6]))
        );
        assert_eq!(
            ticket.ttl_ms(),
            1_500,
            "the screen budget, not the 5 s deadline"
        );
        tickets.push((key.capture, ticket.generation()));
        rig.reply(id, &Reply::Acquired(key, meta(key.capture)))
            .await;
        rig.reply(id, &Reply::Complete(key)).await;
        let held = asking.await.expect("joins").expect("held");
        assert_eq!((held.key, held.generation), (key, Generation(0)));
    }
    assert_eq!(tickets, [(CaptureId(1), 1), (CaptureId(2), 2)]);
}

#[tokio::test]
async fn a_retired_hold_never_reaches_the_worker_again_and_a_later_one_does() {
    let mut rig = fixture::start::<Holds>();
    let client = client(&rig);
    let retiring = tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.retire(owner(7), None, until()).await }
    });
    let (id, request) = rig.next().await;
    assert!(matches!(request, Request::Retire(retired) if retired == owner(7)));
    rig.reply(id, &Reply::Released(owner(7), None)).await;
    assert_eq!(retiring.await.expect("joins"), Ok(()));
    for gesture in [6, 7] {
        let refused = client
            .context(owner(gesture), PhysicalPoint([1, 1]), until())
            .await;
        assert_eq!(
            refused.map(|held| held.key),
            Err(Closed::Refused(Refusal::Stale))
        );
    }
    let _later = tokio::spawn({
        let client = Arc::clone(&client);
        async move {
            client
                .context(owner(8), PhysicalPoint([1, 1]), until())
                .await
        }
    });
    let (_, request) = rig.next().await;
    assert!(
        matches!(request, Request::Context { key, .. } if key.owner == Some(owner(8))),
        "{request:?}"
    );
}

#[tokio::test]
async fn an_answer_from_another_worker_or_for_another_frame_is_lost() {
    let mut rig = fixture::start_at::<Holds>(Generation(1));
    let client = client(&rig);
    let sealing = tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.seal(held(1), until()).await.map(drop) }
    });
    let (id, _) = rig.next().await;
    rig.reply(id, &Reply::Sealed(held(1).key, context())).await;
    assert_eq!(
        sealing.await.expect("joins"),
        Err(Closed::Lost(WorkerLost::Lost)),
        "a replacement's answer"
    );

    let mut rig = fixture::start::<Holds>();
    let client = self::client(&rig);
    for (sealed, expected) in [(2, Err(())), (1, Ok(()))] {
        let sealing = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.seal(held(1), until()).await.map(drop) }
        });
        let (id, request) = rig.next().await;
        assert!(matches!(request, Request::Seal(key) if key == held(1).key));
        rig.reply(id, &Reply::Sealed(held(sealed).key, context()))
            .await;
        assert_eq!(
            sealing.await.expect("joins").map_err(drop),
            expected,
            "sealed for capture {sealed}"
        );
    }
}

#[tokio::test]
async fn a_busy_worker_is_ended_and_only_its_seen_exit_releases_the_hold() {
    for confirmed in [true, false] {
        let mut rig = fixture::start::<Holds>();
        let client = client(&rig);
        let retiring = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.retire(owner(7), Some(Generation(0)), until()).await }
        });
        let (id, _) = rig.next().await;
        rig.reply(id, &Reply::Busy).await;
        rig.held().await;
        rig.confirm.send(confirmed).expect("fixture listens");
        assert_eq!(
            retiring.await.expect("joins"),
            if confirmed { Ok(()) } else { Err(Unreleased) }
        );
    }
}

#[tokio::test]
async fn a_retire_the_full_queue_refuses_ends_the_holder_and_its_exit_releases_the_hold() {
    let mut rig = fixture::start::<Holds>();
    let client = client(&rig);
    let mut seals = Vec::new();
    for _ in 0..16 {
        seals.push(tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.seal(held(1), until()).await.map(drop) }
        }));
        rig.next().await;
    }
    let retiring = tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.retire(owner(7), Some(Generation(0)), until()).await }
    });
    rig.held().await;
    rig.confirm.send(true).expect("fixture listens");
    assert_eq!(retiring.await.expect("joins"), Ok(()));
    for seal in seals {
        assert_eq!(
            seal.await.expect("joins"),
            Err(Closed::Lost(WorkerLost::Lost))
        );
    }
}

#[tokio::test]
async fn a_ticket_runs_out_at_a_sooner_deadline() {
    let mut rig = fixture::start::<Holds>();
    let client = client(&rig);
    let _asking = tokio::spawn({
        let client = Arc::clone(&client);
        async move {
            client
                .context(
                    owner(7),
                    PhysicalPoint([1, 1]),
                    Instant::now() + Duration::from_secs(1),
                )
                .await
        }
    });
    let (_, request) = rig.next().await;
    let Request::Context { ticket, .. } = request else {
        panic!("not a capture: {request:?}")
    };
    assert!(
        (900..=1_000).contains(&ticket.ttl_ms()),
        "{}",
        ticket.ttl_ms()
    );
    #[cfg(windows)]
    assert!(
        crate::platform::qpc_left(ticket.deadline_qpc())
            .is_some_and(|left| left > Duration::from_millis(900) && left <= Duration::from_secs(1))
    );
    #[cfg(not(windows))]
    assert_eq!(ticket.deadline_qpc(), 0);
}

#[tokio::test]
async fn a_replacement_that_answers_anything_but_the_release_is_left_running() {
    let mut rig = fixture::start_at::<Holds>(Generation(1));
    let client = client(&rig);
    let retiring = tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.retire(owner(7), Some(Generation(0)), until()).await }
    });
    let (id, _) = rig.next().await;
    rig.reply(id, &Reply::Busy).await;
    assert_eq!(retiring.await.expect("joins"), Ok(()));
    assert_eq!(
        rig.terminations.load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}
