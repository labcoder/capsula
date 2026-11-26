use std::io;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use kumochi_protocol::constants::SESSION_LOCK;
use kumochi_protocol::message::CAPABILITY_LEN;
use tokio::time::timeout;

use super::*;
use crate::engines::fixture::Fixture;
use crate::platform::SessionLockError;
use crate::runtime::{REFUSE_SPAWN, Selected};
use crate::startup::custody::SessionCustody;
use crate::startup::instance::Added;
use crate::test_support::{Scratch, held_accounts_until};

const PATIENCE: Duration = Duration::from_secs(60);

/// An idle attempt settles without an item call, so none is made.
struct NoItem;

impl InstanceStore for NoItem {
    type Error = std::convert::Infallible;
    fn add(&self, _: &[u8; CAPABILITY_LEN]) -> Result<Added, Self::Error> {
        unreachable!("an idle attempt adds no item")
    }
    fn inspect(&self) -> Result<(), Self::Error> {
        unreachable!("an idle attempt inspects no item")
    }
    fn update(&self, _: &[u8; CAPABILITY_LEN]) -> Result<(), Self::Error> {
        unreachable!("an idle attempt updates no item")
    }
    fn read(&self, _: &mut [u8; CAPABILITY_LEN]) -> Result<(), Self::Error> {
        unreachable!("an idle attempt reads no item")
    }
    fn delete(&self) -> Result<(), Self::Error> {
        unreachable!("an idle attempt deletes no item")
    }
}

/// Holds the credential thread after its vault drops, before it reports the release, until the
/// test sends or drops the other end.
struct Hold(mpsc::Receiver<()>);

impl Drop for Hold {
    fn drop(&mut self) {
        self.0.recv().ok();
    }
}

/// A scratch store, accounts whose release waits on `hold`, and a subscriber scoped to this
/// thread.
struct Scripted<'a> {
    scratch: &'a Scratch,
    hold: Option<Hold>,
}

impl Prerequisites for Scripted<'_> {
    fn ready(&self) -> Result<(), NotReady> {
        Ok(())
    }

    fn init(&mut self) -> Result<ProcessInit, BrainError> {
        let logs = crate::logging::file_subscriber(&self.scratch.dir().join("logs"), "brain")
            .map_err(|_| BrainError::Process(io::ErrorKind::Other.into()))?;
        Ok(ProcessInit {
            _scoped: Some(tracing::subscriber::set_default(logs)),
        })
    }

    fn open(&mut self) -> Result<OpenedState, BrainError> {
        OpenedState::load(
            &self.scratch.db(),
            || Some(self.scratch.dir().to_owned()),
            Some(Selected::Fixture(Box::new(
                Fixture::parse("").expect("empty fixture"),
            ))),
        )
    }

    fn accounts(
        &mut self,
        _: OAuthTuning,
        _: TokenTuning,
        profile: Arc<SessionLock>,
    ) -> Result<(Accounts, VaultHeld), BrainError> {
        let hold = self.hold.take().expect("one start");
        Ok((held_accounts_until(profile, hold), Arc::default()))
    }
}

/// Fails a root's start after its accounts started, at `stop` or else in `Brain::new`, and
/// checks that settling waits for the credential thread's release before custody drops.
async fn assert_settles_after_the_release(name: &str, stop: Option<Checkpoint>) {
    let scratch = Scratch::new(name);
    let lock = scratch.dir().join(SESSION_LOCK);
    let token = Arc::new(());
    let custody = Arc::downgrade(&token);
    let mut root = Root::new();
    root.instance = Some(InstanceAttempt::new(
        SessionCustody::fixture(token),
        Arc::new(NoItem),
    ));
    let (free, held) = mpsc::channel();
    let mut pre = Scripted {
        scratch: &scratch,
        hold: Some(Hold(held)),
    };
    REFUSE_SPAWN.set(stop.is_none());
    let Err(error) = root.start(&mut pre, |point, _| {
        if Some(point) == stop {
            Err(BrainError::Process(io::ErrorKind::Interrupted.into()))
        } else {
            Ok(())
        }
    }) else {
        panic!("the injected failure must stop the start");
    };
    match stop {
        Some(_) => assert!(
            matches!(&error, BrainError::Process(error) if error.kind() == io::ErrorKind::Interrupted),
            "{error:?}"
        ),
        None => assert!(
            matches!(&error, BrainError::StoreThread(error) if error.kind() == io::ErrorKind::Other),
            "{error:?}"
        ),
    }
    assert!(root.stages.brain.is_none(), "no Brain was built");
    let settle = root.settle();
    tokio::pin!(settle);
    assert!(
        timeout(Duration::from_millis(200), &mut settle)
            .await
            .is_err(),
        "settling waits for the held credential thread"
    );
    assert!(
        matches!(SessionLock::try_acquire(&lock), Err(SessionLockError::Busy)),
        "a contender is refused while the release is held"
    );
    assert!(custody.upgrade().is_some(), "custody outlives the release");
    free.send(())
        .expect("the credential thread waits for its release");
    let settled = timeout(PATIENCE, settle)
        .await
        .expect("settling ends once the release is free");
    assert_eq!(
        settled,
        Settled {
            runtime: Shutdown {
                quit: None,
                store: Stage::Unconfirmed,
                settings: Stage::Confirmed,
                claude: Stage::Confirmed,
                account: Stage::Confirmed,
            },
            item: Stage::Confirmed,
        },
        "the accounts close in time and the raw store stays unconfirmed"
    );
    assert!(custody.upgrade().is_none(), "custody drops at settlement");
    drop(SessionLock::try_acquire(&lock).expect("the profile is free once settled"));
}

#[tokio::test]
async fn a_failed_accounts_checkpoint_closes_the_accounts_before_custody_drops() {
    Box::pin(assert_settles_after_the_release(
        "root-accounts-checkpoint",
        Some(Checkpoint::Accounts),
    ))
    .await;
}

#[tokio::test]
async fn a_failed_brain_constructor_returns_the_accounts_to_close_before_custody_drops() {
    Box::pin(assert_settles_after_the_release("root-brain-new", None)).await;
}

#[test]
fn this_builds_prerequisites_are_never_ready() {
    assert_eq!(Pending.ready(), Err(NotReady));
}
