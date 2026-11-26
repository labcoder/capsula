//! What each stage of the Brain's shutdown reports, read from its real receipts: the store's
//! close, the settings writes owed from shutdown's start, Claude Code's lane and the accounts.

use std::sync::Arc;
use std::time::Duration;

use kumochi_protocol::constants::{CONFIG_FILE, DEFAULT_ASSISTANT_NAME};
use kumochi_protocol::message::{AssistantName, ControlToBrain, SettingsOp};
use tokio::time::{Instant, timeout};

use super::PATIENCE;
use crate::engines::fixture::Fixture;
use crate::platform::{SessionLock, SessionLockError};
use crate::runtime::settings::Gate;
use crate::runtime::{Brain, Quit, Selected, Shutdown, Stage};
use crate::store::Store;
use crate::tap::Tap;
use crate::test_support::{self, SENTINEL, Scratch};

const CONFIRMED: Shutdown = Shutdown {
    quit: Some(Quit::Complete),
    store: Stage::Confirmed,
    settings: Stage::Confirmed,
    claude: Stage::Confirmed,
    account: Stage::Confirmed,
};

/// Queues a rename with no shell connected; nothing stages it until the next pump.
fn rename(brain: &mut Brain, name: &str) {
    let op = SettingsOp::SetAssistantName(AssistantName::new(name).expect("valid"));
    brain.control_offline(ControlToBrain::Settings(op), 0, &mut Tap::from_env());
}

fn saved_name(scratch: &Scratch) -> String {
    scratch.config().view().assistant_name.as_str().to_owned()
}

/// A brain whose writer lock is `profile`, so a test can watch who still holds it.
fn brain_under(scratch: &Scratch, profile: Arc<SessionLock>) -> Brain {
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    let main_thread = store.main_thread().expect("main thread");
    let high_water = store.high_water().expect("high water");
    let scenes = store.scenes().expect("scenes");
    Brain::new(
        store,
        profile,
        main_thread,
        high_water,
        scenes,
        Selected::Fixture(Box::new(Fixture::parse("").expect("script"))),
        scratch.config(),
        test_support::accounts(scratch),
        crate::runtime::IssueEpochs::default(),
    )
    .expect("brain")
}

#[tokio::test]
async fn a_quit_whose_stages_all_answer_in_time_is_confirmed() {
    let scratch = Scratch::new("shutdown-confirmed");
    let mut brain = test_support::brain(&scratch, "");
    rename(&mut brain, "Nimbus");
    assert_eq!(brain.quit(Instant::now() + PATIENCE).await, CONFIRMED);
    assert_eq!(saved_name(&scratch), "Nimbus");
}

#[tokio::test]
async fn a_close_reports_its_stages_and_no_quit() {
    let scratch = Scratch::new("shutdown-close");
    let mut brain = test_support::brain(&scratch, "");
    rename(&mut brain, "Nimbus");
    assert_eq!(
        brain.close().await,
        Shutdown {
            quit: None,
            ..CONFIRMED
        }
    );
    assert_eq!(saved_name(&scratch), "Nimbus");
}

/// `timeout_at` polls a ready stage before its timer, so only the clock tells it was late. No
/// settings write was owed, so there's no save to be late.
#[tokio::test]
async fn a_success_seen_at_an_expired_deadline_is_late() {
    let scratch = Scratch::new("shutdown-expired");
    let brain = test_support::brain(&scratch, "");
    let shutdown = brain.quit(Instant::now()).await;
    // Claude Code has no lane, so it's ready on its first poll.
    assert_eq!(
        (shutdown.settings, shutdown.claude, shutdown.account),
        (Stage::Confirmed, Stage::Late, Stage::Late),
        "{shutdown:?}"
    );
}

#[tokio::test]
async fn a_write_that_finished_before_an_expired_deadline_is_read_as_late() {
    let scratch = Scratch::new("shutdown-late-write");
    let mut brain = test_support::brain(&scratch, "");
    let release = brain.hold_config_write();
    rename(&mut brain, "Nimbus");
    brain.pump_settings_offline();
    let (written_tx, written) = tokio::sync::oneshot::channel();
    release
        .send(Gate::WriteAndSignal(written_tx))
        .expect("the write waits on the gate");
    assert!(
        timeout(PATIENCE, written)
            .await
            .expect("the write ends")
            .expect("it reports"),
        "the write saved"
    );
    let finished = async {
        while !brain
            .config_write
            .as_ref()
            .expect("the write is still unread")
            .diagnostics()
            .1
        {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    timeout(PATIENCE, finished)
        .await
        .expect("the write's task ends");
    let shutdown = brain.quit(Instant::now()).await;
    assert_eq!(shutdown.settings, Stage::Late, "{shutdown:?}");
    assert_eq!(saved_name(&scratch), "Nimbus");
}

/// The refusal comes from shutdown's own first pump, so a latch started after it would miss it.
#[tokio::test]
async fn a_change_refused_as_shutdown_stages_it_fails_the_settings() {
    let scratch = Scratch::new("shutdown-refused");
    let config = scratch.db().with_file_name(CONFIG_FILE);
    std::fs::create_dir_all(config.parent().expect("a folder")).expect("folder");
    std::fs::write(&config, "not = [toml").expect("broken");
    let mut brain = test_support::brain(&scratch, "");
    rename(&mut brain, "Nimbus");
    assert_eq!(
        brain.quit(Instant::now() + PATIENCE).await,
        Shutdown {
            settings: Stage::Failed,
            ..CONFIRMED
        }
    );
}

#[tokio::test]
async fn a_write_that_fails_at_shutdown_fails_the_settings() {
    let scratch = Scratch::new("shutdown-write-failed");
    let mut brain = test_support::brain(&scratch, "");
    // A folder where the temp file goes makes the write fail before the replace.
    let blocker = scratch.db().with_file_name(format!("{CONFIG_FILE}.tmp"));
    std::fs::create_dir_all(&blocker).expect("blocker");
    rename(&mut brain, "Nimbus");
    assert_eq!(
        brain.quit(Instant::now() + PATIENCE).await,
        Shutdown {
            settings: Stage::Failed,
            ..CONFIRMED
        }
    );
    assert_eq!(saved_name(&scratch), DEFAULT_ASSISTANT_NAME);
}

#[tokio::test]
async fn a_failed_write_stays_failed_after_a_later_save() {
    let scratch = Scratch::new("shutdown-failed-then-saved");
    let mut brain = test_support::brain(&scratch, "");
    let release = brain.hold_config_write();
    rename(&mut brain, "Nimbus");
    rename(&mut brain, "Cirrus");
    release
        .send(Gate::Panic)
        .expect("the gate waits for the write");
    assert_eq!(
        brain.quit(Instant::now() + PATIENCE).await,
        Shutdown {
            settings: Stage::Failed,
            ..CONFIRMED
        }
    );
    assert_eq!(saved_name(&scratch), "Cirrus");
}

#[tokio::test]
async fn a_write_that_panics_at_shutdown_fails_the_settings() {
    let scratch = Scratch::new("shutdown-write-panicked");
    let mut brain = test_support::brain(&scratch, "");
    let release = brain.hold_config_write();
    rename(&mut brain, "Nimbus");
    release
        .send(Gate::Panic)
        .expect("the gate waits for the write");
    assert_eq!(
        brain.quit(Instant::now() + PATIENCE).await,
        Shutdown {
            settings: Stage::Failed,
            ..CONFIRMED
        }
    );
    assert_eq!(saved_name(&scratch), DEFAULT_ASSISTANT_NAME);
}

#[tokio::test]
async fn a_write_held_past_shutdown_keeps_the_profile_locked_until_it_stops() {
    let scratch = Scratch::new("shutdown-held-write");
    let path = scratch.db().with_file_name("profile.lock");
    let profile = Arc::new(SessionLock::try_acquire(&path).expect("profile lock"));
    let holders = Arc::downgrade(&profile);
    let mut brain = brain_under(&scratch, profile);
    let release = brain.hold_config_write();
    rename(&mut brain, "Nimbus");
    brain.pump_settings_offline();
    assert_eq!(
        holders.strong_count(),
        3,
        "the brain, the store thread and the held write each keep the profile"
    );
    let shutdown = brain
        .quit(Instant::now() + Duration::from_millis(100))
        .await;
    assert_eq!(shutdown.settings, Stage::Outstanding, "{shutdown:?}");
    // The store thread lets its clone go once its helpers join; the held write keeps its own.
    let patience = Instant::now() + PATIENCE;
    while holders.strong_count() > 1 && Instant::now() < patience {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        holders.strong_count(),
        1,
        "only the held write keeps the profile past the brain and its deadline"
    );
    assert!(
        matches!(SessionLock::try_acquire(&path), Err(SessionLockError::Busy)),
        "the profile stays locked while the write may still replace the file"
    );
    let (written_tx, written) = tokio::sync::oneshot::channel();
    release
        .send(Gate::WriteAndSignal(written_tx))
        .expect("the write waits on the gate");
    assert!(
        timeout(PATIENCE, written)
            .await
            .expect("the write ends")
            .expect("it reports"),
        "the late write saved"
    );
    assert_eq!(
        holders.strong_count(),
        0,
        "the write let the profile go as it stopped"
    );
    drop(SessionLock::try_acquire(&path).expect("the profile is free once the write stops"));
    assert_eq!(saved_name(&scratch), "Nimbus");
}

#[tokio::test]
async fn a_brains_debug_never_shows_its_profile_lock() {
    let scratch = Scratch::new("shutdown-debug");
    let path = scratch.db().with_file_name(format!("{SENTINEL}.lock"));
    let profile = Arc::new(SessionLock::try_acquire(&path).expect("profile lock"));
    let brain = brain_under(&scratch, profile);
    let printed = format!("{brain:?}");
    let bytes = format!("{:?}", SENTINEL.as_bytes());
    assert!(!printed.contains(SENTINEL), "the lock's path was printed");
    assert!(
        !printed.contains(bytes.trim_matches(['[', ']'])),
        "the lock's path was printed as bytes"
    );
    brain.close().await;
}
