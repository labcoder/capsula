use std::io;
use std::time::Duration;

use super::*;
use crate::engines::fixture::Fixture;
use crate::test_support::{Scratch, accounts};

#[tokio::test]
async fn an_error_after_open_state_drops_the_store_and_pipes() {
    assert_unwind(Checkpoint::Open, 0).await;
}

#[tokio::test]
async fn an_error_after_build_brain_releases_the_writer_and_pipes() {
    assert_unwind(Checkpoint::Brain, 1).await;
}

#[tokio::test]
async fn an_error_after_start_helpers_drops_the_job_without_shutting_it_down() {
    assert_unwind(Checkpoint::Helpers, 2).await;
}

async fn assert_unwind(stop: Checkpoint, slot: u32) {
    let scratch = Scratch::new("startup-unwind");
    let session = u32::MAX - std::process::id() - 64 - slot;
    let lifecycle = Lifecycle::acquire(session).expect("own test pipes");
    let replay = Selected::Fixture(Box::new(Fixture::parse("").expect("empty fixture")));
    let opened = OpenedState::load(
        &scratch.db(),
        || Some(scratch.dir().to_owned()),
        Some(replay),
    )
    .expect("open and recover the test store");
    let side = Arc::downgrade(&opened.store.side());
    let mut writer = None;
    let mut vault = None;
    let mut job = None;
    let Err(error) = lifecycle.start(
        opened,
        |_, _| {
            let mut account = accounts(&scratch);
            vault = account.take_release_receipt();
            Ok((account, Arc::default()))
        },
        |point, state| {
            if point != stop {
                return Ok(());
            }
            if let Some(brain) = state.brain.as_mut() {
                writer = Some(brain.take_store_close_receipt());
            }
            job.clone_from(&state.jobs);
            Err(BrainError::Process(io::ErrorKind::Interrupted.into()))
        },
    ) else {
        panic!("the injected failure must stop the startup");
    };
    assert!(
        matches!(error, BrainError::Process(error) if error.kind() == io::ErrorKind::Interrupted),
        "the startup must return the injected error unchanged"
    );
    let _pipes = Pipes::create(session).expect("both pipe names are reusable after refusal");
    if stop == Checkpoint::Open {
        assert!(
            writer.is_none() && vault.is_none(),
            "account and writer were not started"
        );
    } else {
        tokio::time::timeout(Duration::from_secs(5), writer.expect("writer started"))
            .await
            .expect("the disconnected writer finishes")
            .expect("the writer really closed the store");
        tokio::time::timeout(Duration::from_secs(5), vault.expect("account started"))
            .await
            .expect("the disconnected credential worker finishes")
            .expect("the credential worker released its session");
    }
    assert!(
        side.upgrade().is_none(),
        "no owner retains the opened store"
    );
    if stop == Checkpoint::Helpers {
        let mut job = job.expect("helper Job started");
        let job = Arc::get_mut(&mut job).expect("startup released its Job reference");
        assert!(
            !job.is_shut_down(),
            "partial startup does not explicitly shut down the Job"
        );
    } else {
        assert!(job.is_none(), "helper Job was not started");
    }
}
