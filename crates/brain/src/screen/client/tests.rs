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
        owner: owner(7),
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
async fn each_capture_gets_a_fresh_key_and_a_higher_ticket_and_names_its_worker() {
    let mut rig = fixture::start::<Holds>();
    let client = client(&rig);
    let mut tickets = Vec::new();
    for gesture in [7, 8] {
        let asking = tokio::spawn({
            let client = Arc::clone(&client);
            async move {
                client
                    .context(
                        owner(gesture),
                        PhysicalPoint([5, 6]),
                        CaptureDeadline::after(Duration::from_secs(5)),
                    )
                    .await
            }
        });
        let (id, request) = rig.next().await;
        let Request::Context { key, ticket, point } = request else {
            panic!("not a capture: {request:?}")
        };
        assert_eq!(
            (key.owner, ticket.capture(), point),
            (owner(gesture), key.capture, PhysicalPoint([5, 6]))
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
            .context(
                owner(gesture),
                PhysicalPoint([1, 1]),
                CaptureDeadline::after(Duration::from_secs(5)),
            )
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
                .context(
                    owner(8),
                    PhysicalPoint([1, 1]),
                    CaptureDeadline::after(Duration::from_secs(5)),
                )
                .await
        }
    });
    let (_, request) = rig.next().await;
    assert!(
        matches!(request, Request::Context { key, .. } if key.owner == owner(8)),
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
    let deadline = CaptureDeadline::after(Duration::from_secs(1));
    let _asking = tokio::spawn({
        let client = Arc::clone(&client);
        async move {
            client
                .context(owner(7), PhysicalPoint([1, 1]), deadline)
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
    assert_eq!(ticket.deadline_qpc(), deadline.stamp);
    #[cfg(any(windows, target_os = "macos"))]
    assert!(
        crate::platform::qpc_left(ticket.deadline_qpc())
            .is_some_and(|left| left > Duration::from_millis(900) && left <= Duration::from_secs(1))
    );
    #[cfg(not(any(windows, target_os = "macos")))]
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
