use std::io;
use std::sync::atomic::AtomicU8;
use std::time::Duration;

use super::*;
use crate::engines::fixture::Fixture;
use kumochi_protocol::constants::SESSION_LOCK;

use crate::platform::{SessionLockError, ShellProcess};
use crate::test_support::{Scratch, held_accounts};

const POINTS: [Checkpoint; 4] = [
    Checkpoint::Open,
    Checkpoint::Accounts,
    Checkpoint::Brain,
    Checkpoint::Helpers,
];

#[tokio::test]
async fn an_error_after_open_state_drops_the_store_and_pipes() {
    assert_unwind(Checkpoint::Open, 0).await;
}

#[tokio::test]
async fn an_error_after_start_accounts_releases_the_credential_worker_and_pipes() {
    assert_unwind(Checkpoint::Accounts, 3).await;
}

#[tokio::test]
async fn an_error_after_build_brain_releases_the_writer_and_pipes() {
    assert_unwind(Checkpoint::Brain, 1).await;
}

#[tokio::test]
async fn an_error_after_start_helpers_drops_the_job_without_shutting_it_down() {
    assert_unwind(Checkpoint::Helpers, 2).await;
}

/// A failed parent fence, mapped to the error the injected unwinds use.
fn refused(shell: &ShellProcess) -> Result<(), BrainError> {
    shell
        .fence()
        .map_err(|_| BrainError::Process(io::ErrorKind::Interrupted.into()))
}

fn lifecycle(slot: u32) -> Lifecycle {
    Lifecycle {
        jobs: None,
        brain: None,
        opened: None,
        profile: None,
        exe: std::env::current_exe().expect("current test executable"),
        pipes: Pipes::create_for_startup_test(slot).expect("own test pipes"),
        session: 0,
        published: false,
    }
}

fn replay() -> Selected {
    Selected::Fixture(Box::new(Fixture::parse("").expect("empty fixture")))
}

async fn assert_unwind(stop: Checkpoint, slot: u32) {
    unwind(stop, slot, None).await;
}

/// Stops startup at `stop`: an injected error, or with a claimed `shell`, a loss published there
/// and refused by that point's fence after every earlier fence admitted its unit.
async fn unwind(stop: Checkpoint, slot: u32, shell: Option<&ShellProcess>) {
    let scratch = Scratch::new("startup-unwind");
    let lifecycle = lifecycle(slot);
    let opened = OpenedState::load(
        &scratch.db(),
        || Some(scratch.dir().to_owned()),
        Some(replay()),
    )
    .expect("open and recover the test store");
    let side = Arc::downgrade(&opened.store.side());
    let mut writer = None;
    let mut vault = None;
    let mut job = None;
    let mut fenced = Vec::new();
    let Err(error) = lifecycle.start(
        Mode::Detached,
        opened,
        |_, _, profile| {
            let mut account = held_accounts(&scratch, profile);
            vault = account.take_release_receipt();
            Ok((account, Arc::default()))
        },
        |point, state| {
            if point == stop {
                if let Some(brain) = state.brain.as_mut() {
                    writer = Some(brain.take_store_close_receipt());
                }
                job.clone_from(&state.jobs);
            }
            let Some(shell) = shell else {
                return if point == stop {
                    Err(BrainError::Process(io::ErrorKind::Interrupted.into()))
                } else {
                    Ok(())
                };
            };
            if point == stop {
                shell.publish(1);
            }
            fenced.push(point);
            refused(shell)
        },
    ) else {
        panic!("the injected failure must stop the startup");
    };
    assert!(
        matches!(error, BrainError::Process(error) if error.kind() == io::ErrorKind::Interrupted),
        "the startup must return the injected error unchanged"
    );
    let _pipes =
        Pipes::create_for_startup_test(slot).expect("both pipe names are reusable after refusal");
    if shell.is_some() {
        let reached = POINTS
            .iter()
            .position(|point| *point == stop)
            .expect("a point")
            + 1;
        assert_eq!(
            fenced,
            POINTS[..reached],
            "each earlier fence admitted one unit"
        );
    }
    if stop == Checkpoint::Open {
        assert!(
            writer.is_none() && vault.is_none(),
            "account and writer were not started"
        );
    } else {
        if stop == Checkpoint::Accounts {
            assert!(writer.is_none(), "the Brain was not built");
        } else {
            tokio::time::timeout(Duration::from_secs(5), writer.expect("writer started"))
                .await
                .expect("the disconnected writer finishes")
                .expect("the writer really closed the store");
        }
        tokio::time::timeout(Duration::from_secs(5), vault.expect("account started"))
            .await
            .expect("the disconnected credential worker finishes")
            .expect("the credential worker released its session");
    }
    assert!(
        side.upgrade().is_none(),
        "no owner retains the opened store"
    );
    assert_profile_free(&scratch).await;
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

#[tokio::test]
async fn a_loss_after_the_claim_fails_the_next_fence_and_unwinds_like_an_error() {
    for stop in POINTS {
        let shell = ShellProcess::fixture(9, Arc::new(AtomicU8::new(0)));
        assert!(shell.claim_commit().is_some());
        unwind(stop, 4, Some(&shell)).await;
    }
}

#[tokio::test]
async fn a_loss_while_the_state_opens_lets_it_finish_and_refuses_the_accounts() {
    let scratch = Scratch::new("startup-loss-open");
    let shell = ShellProcess::fixture(9, Arc::new(AtomicU8::new(0)));
    assert!(shell.claim_commit().is_some());
    let opened = OpenedState::load(
        &scratch.db(),
        || {
            shell.publish(1);
            Some(scratch.dir().to_owned())
        },
        Some(replay()),
    )
    .expect("the admitted unit recovers the store and loads the settings after the loss");
    let side = Arc::downgrade(&opened.store.side());
    let mut accounts = false;
    let mut fenced = Vec::new();
    let Err(error) = lifecycle(5).start(
        Mode::Detached,
        opened,
        |_, _, _| {
            accounts = true;
            Err(BrainError::Process(io::ErrorKind::Other.into()))
        },
        |point, _| {
            fenced.push(point);
            refused(&shell)
        },
    ) else {
        panic!("the fence after the unit must refuse");
    };
    assert!(
        matches!(error, BrainError::Process(error) if error.kind() == io::ErrorKind::Interrupted)
    );
    assert_eq!(fenced, [Checkpoint::Open]);
    assert!(!accounts, "no accounts worker starts");
    assert!(
        side.upgrade().is_none(),
        "no owner retains the opened store"
    );
    assert_profile_free(&scratch).await;
}

#[tokio::test]
async fn a_loss_while_the_accounts_start_refuses_the_brain() {
    let scratch = Scratch::new("startup-loss-accounts");
    let shell = ShellProcess::fixture(9, Arc::new(AtomicU8::new(0)));
    assert!(shell.claim_commit().is_some());
    let opened = OpenedState::load(
        &scratch.db(),
        || Some(scratch.dir().to_owned()),
        Some(replay()),
    )
    .expect("open and recover the test store");
    let mut vault = None;
    let mut brain = false;
    let mut fenced = Vec::new();
    let Err(error) = lifecycle(6).start(
        Mode::Detached,
        opened,
        |_, _, profile| {
            let mut account = held_accounts(&scratch, profile);
            vault = account.take_release_receipt();
            shell.publish(1);
            Ok((account, Arc::default()))
        },
        |point, state| {
            brain |= state.brain.is_some();
            fenced.push(point);
            refused(&shell)
        },
    ) else {
        panic!("the fence after the accounts must refuse");
    };
    assert!(
        matches!(error, BrainError::Process(error) if error.kind() == io::ErrorKind::Interrupted)
    );
    assert_eq!(fenced, [Checkpoint::Open, Checkpoint::Accounts]);
    assert!(!brain, "no Brain is built");
    tokio::time::timeout(Duration::from_secs(5), vault.expect("account started"))
        .await
        .expect("the disconnected credential worker finishes")
        .expect("the credential worker released its session");
    assert_profile_free(&scratch).await;
}

async fn assert_profile_free(scratch: &Scratch) {
    let lock = scratch.dir().join(SESSION_LOCK);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match SessionLock::try_acquire(&lock) {
            Ok(_) => break,
            Err(SessionLockError::Busy) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(error) => panic!("the unwind kept the profile lock: {error}"),
        }
    }
}
