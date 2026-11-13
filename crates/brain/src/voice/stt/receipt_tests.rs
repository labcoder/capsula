use std::future::Future as _;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};

use super::*;
use crate::supervise::{Dispatch, Kind, fixture};

struct Test;

impl Kind for Test {
    const ARG: &'static str = "--test";
    const NAME: &'static str = "test";
    const MAX_REQUEST: usize = 1024;
    const MAX_REPLY: usize = 1024;
    type Request = Vec<u8>;
    type Reply = String;
    type Assembly = crate::supervise::Assembly;
}

#[tokio::test]
async fn a_closed_retirement_receiver_ends_pending_and_dispatched_waits_with_a_live_broadcast_sender()
 {
    for state in [Dispatch::Pending, Dispatch::Dispatched(Generation(0))] {
        let (retirement, intents) = fixture::retirement();
        let (_dispatch, mut receipt) = watch::channel(state);
        let (_live_sender, mut exits) = broadcast::channel(4);
        let mut release = Box::pin(released(&mut receipt, &mut exits, &retirement));
        let mut context = Context::from_waker(Waker::noop());
        assert_eq!(release.as_mut().poll(&mut context), Poll::Pending);
        drop(intents);
        assert_eq!(
            release.as_mut().poll(&mut context),
            Poll::Ready(Err(SpeechError::ExitUnconfirmed))
        );
    }
}

#[tokio::test]
async fn final_actor_exit_is_published_before_lane_closure_and_only_true_confirmation_releases() {
    for confirmed in [false, true] {
        let mut rig = fixture::start::<Test>();
        let retirement = rig.handle.retirement();
        let _keep_broadcast_open = rig.handle.test_exits();
        let mut exits = rig.handle.exits();
        let mut release = Box::pin(super::confirmed(&mut exits, Generation(0), &retirement));
        let mut context = Context::from_waker(Waker::noop());
        assert_eq!(release.as_mut().poll(&mut context), Poll::Pending);
        rig.held().await;
        assert_eq!(release.as_mut().poll(&mut context), Poll::Pending);
        drop(rig.handle);
        rig.confirm.send(confirmed).expect("held physical result");
        rig.actor.await.expect("actor finishes after final Exit");
        assert_eq!(
            release.await,
            if confirmed {
                Ok(())
            } else {
                Err(SpeechError::ExitUnconfirmed)
            }
        );
        assert_eq!(rig.terminations.load(Ordering::Relaxed), 1);
        assert_eq!(rig.spawns.load(Ordering::Relaxed), 0);
    }
}
