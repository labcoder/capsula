use super::*;
use std::sync::atomic::{AtomicU8, AtomicUsize};
use std::sync::{Mutex, mpsc};
use std::time::Duration;

use crate::platform::shell_process::Unconfirmed;

const KEY: [u8; CAPABILITY_LEN] = [0xA5; CAPABILITY_LEN];
const OTHER: [u8; CAPABILITY_LEN] = [0x11; CAPABILITY_LEN];

/// The one exact item's state in the fake keychain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Item {
    Absent,
    Plain([u8; CAPABILITY_LEN]),
    Acl([u8; CAPABILITY_LEN]),
    Migratable([u8; CAPABILITY_LEN]),
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    Missing,
    Absent,
    Unavailable,
    Ineligible,
    Failed,
}

#[derive(Default)]
struct Script {
    fail: Vec<Step>,
    fail_once: Vec<Step>,
    flips: usize,
    hold: Vec<(Step, mpsc::Sender<()>, mpsc::Receiver<()>)>,
}

/// A keychain holding one item, whose calls a test can fail, hold or corrupt.
struct Fake {
    item: Mutex<Item>,
    calls: AtomicUsize,
    script: Mutex<Script>,
}

impl Fake {
    fn new(item: Item) -> Arc<Self> {
        Arc::new(Self {
            item: Mutex::new(item),
            calls: AtomicUsize::new(0),
            script: Mutex::default(),
        })
    }

    fn item(&self) -> Item {
        *self.item.lock().expect("item")
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn fail(&self, step: Step) {
        self.script.lock().expect("script").fail.push(step);
    }

    fn fail_once(&self, step: Step) {
        self.script.lock().expect("script").fail_once.push(step);
    }

    fn flip_read(&self) {
        self.script.lock().expect("script").flips = usize::MAX;
    }

    fn flip_next_read(&self) {
        self.script.lock().expect("script").flips = 1;
    }

    /// Holds the next `step` call: the first receiver hears it start, the sender releases it.
    fn hold(&self, step: Step) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        self.script
            .lock()
            .expect("script")
            .hold
            .push((step, entered, released));
        (started, release)
    }

    fn call(&self, step: Step) -> Result<(), Fault> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let held = {
            let mut script = self.script.lock().expect("script");
            let at = script.hold.iter().position(|(held, ..)| *held == step);
            at.map(|at| script.hold.remove(at))
        };
        if let Some((_, entered, released)) = held {
            entered.send(()).expect("the test listens");
            released.recv().expect("the test releases");
        }
        let mut script = self.script.lock().expect("script");
        if let Some(at) = script.fail_once.iter().position(|once| *once == step) {
            script.fail_once.remove(at);
            return Err(Fault::Failed);
        }
        if script.fail.contains(&step) {
            return Err(Fault::Failed);
        }
        Ok(())
    }
}

impl InstanceStore for Fake {
    type Error = Fault;

    fn add(&self, key: &[u8; CAPABILITY_LEN]) -> Result<Added, Fault> {
        self.call(Step::Add)?;
        let mut item = self.item.lock().expect("item");
        if *item != Item::Absent {
            return Ok(Added::Duplicate);
        }
        *item = Item::Plain(*key);
        Ok(Added::Fresh)
    }

    fn inspect(&self) -> Result<(), Fault> {
        self.call(Step::Inspect)?;
        match self.item() {
            Item::Plain(_) => Ok(()),
            Item::Absent => Err(Fault::Missing),
            Item::Acl(_) | Item::Migratable(_) => Err(Fault::Ineligible),
            Item::Unavailable => Err(Fault::Unavailable),
        }
    }

    fn update(&self, key: &[u8; CAPABILITY_LEN]) -> Result<(), Fault> {
        self.call(Step::Update)?;
        let mut item = self.item.lock().expect("item");
        match *item {
            Item::Plain(_) => {
                *item = Item::Plain(*key);
                Ok(())
            }
            Item::Absent => Err(Fault::Failed),
            _ => Err(Fault::Ineligible),
        }
    }

    fn read(&self, into: &mut [u8; CAPABILITY_LEN]) -> Result<(), Fault> {
        self.call(Step::Read)?;
        match self.item() {
            Item::Plain(bytes) => {
                *into = bytes;
                let mut script = self.script.lock().expect("script");
                if script.flips > 0 {
                    script.flips -= 1;
                    into[CAPABILITY_LEN - 1] ^= 1;
                }
                Ok(())
            }
            Item::Absent => Err(Fault::Missing),
            Item::Unavailable => Err(Fault::Unavailable),
            Item::Acl(_) | Item::Migratable(_) => Err(Fault::Ineligible),
        }
    }

    fn delete(&self) -> Result<(), Fault> {
        self.call(Step::Delete)?;
        let mut item = self.item.lock().expect("item");
        if *item == Item::Absent {
            return Err(Fault::Absent);
        }
        *item = Item::Absent;
        Ok(())
    }
}

/// An attempt under fixture custody, and a handle that sees whether that custody is still held.
fn start(store: &Arc<Fake>) -> (InstanceAttempt<Fake>, std::sync::Weak<()>) {
    let token = Arc::new(());
    let held = Arc::downgrade(&token);
    let custody = SessionCustody::fixture(token);
    (InstanceAttempt::new(custody, Arc::clone(store)), held)
}

/// Per buffer the attempt made, in order: None while it's alive, else whether it was all zero
/// when dropped.
fn wiped(attempt: &InstanceAttempt<Fake>) -> Vec<Option<bool>> {
    wipes::of(&attempt.made)
}

fn key() -> Key {
    Zeroizing::new(KEY)
}

fn shell() -> ShellProcess {
    ShellProcess::fixture(7, Arc::new(AtomicU8::new(0)))
}

fn later() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

/// Releases a held call from another thread once it starts, after `first` runs there.
fn release_after(
    started: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
    first: impl FnOnce() + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        started.recv().expect("the held call starts");
        first();
        release.send(()).expect("the call waits");
    })
}

#[tokio::test]
async fn a_fresh_add_publishes_once_and_the_published_attempt_holds_no_bytes() {
    let store = Fake::new(Item::Absent);
    let (mut attempt, _held) = start(&store);
    assert_eq!(attempt.publish(&key(), later(), &shell()).await, Ok(()));
    assert_eq!(store.item(), Item::Plain(KEY));
    assert_eq!(
        wiped(&attempt),
        [Some(true), Some(true)],
        "the copy and the readback are wiped while Published lives"
    );
    assert_eq!(
        attempt.publish(&key(), later(), &shell()).await,
        Err(Refused::Used)
    );
    let shown = format!("{attempt:?}");
    assert_eq!(shown, r#"InstanceAttempt { state: "Published", .. }"#);
    assert_eq!(store.calls(), 2, "one add and one readback");
}

#[tokio::test]
async fn a_plain_duplicate_is_inspected_then_updated_and_read_back() {
    let store = Fake::new(Item::Plain(OTHER));
    let (mut attempt, _held) = start(&store);
    assert_eq!(attempt.publish(&key(), later(), &shell()).await, Ok(()));
    assert_eq!(store.item(), Item::Plain(KEY));
    assert_eq!(wiped(&attempt), [Some(true), Some(true)]);
}

#[tokio::test]
async fn an_ineligible_duplicate_refuses_without_update_or_delete() {
    for (seed, fault) in [
        (Item::Acl(OTHER), Fault::Ineligible),
        (Item::Migratable(OTHER), Fault::Ineligible),
        (Item::Unavailable, Fault::Unavailable),
    ] {
        let store = Fake::new(seed);
        let (mut attempt, _held) = start(&store);
        assert_eq!(
            attempt.publish(&key(), later(), &shell()).await,
            Err(Refused::Store(Step::Inspect, fault)),
            "{seed:?}"
        );
        assert_eq!(store.item(), seed, "unchanged");
        assert_eq!(wiped(&attempt), [Some(true)], "{seed:?}: refusal wipes");
        assert_eq!(
            attempt.publish(&key(), later(), &shell()).await,
            Err(Refused::Used),
            "a refusal uses the attempt up"
        );
    }
}

#[tokio::test]
async fn a_readback_that_differs_or_fails_refuses_and_deletes_nothing() {
    let store = Fake::new(Item::Absent);
    store.flip_read();
    let (mut attempt, _held) = start(&store);
    assert_eq!(
        attempt.publish(&key(), later(), &shell()).await,
        Err(Refused::Mismatch)
    );
    assert_eq!(store.item(), Item::Plain(KEY));
    assert_eq!(wiped(&attempt), [Some(true), Some(true)]);

    let store = Fake::new(Item::Absent);
    store.fail(Step::Read);
    let (mut attempt, _held) = start(&store);
    assert_eq!(
        attempt.publish(&key(), later(), &shell()).await,
        Err(Refused::Store(Step::Read, Fault::Failed))
    );
    assert_eq!(store.item(), Item::Plain(KEY));
}

#[tokio::test]
async fn a_passed_deadline_or_a_loss_on_entry_starts_no_call() {
    let store = Fake::new(Item::Absent);
    let (mut attempt, _held) = start(&store);
    assert_eq!(
        attempt.publish(&key(), Instant::now(), &shell()).await,
        Err(Refused::Deadline)
    );
    let lost = ShellProcess::fixture(7, Arc::new(AtomicU8::new(1)));
    let (mut second, _held) = start(&store);
    assert_eq!(
        second.publish(&key(), later(), &lost).await,
        Err(Refused::Lost(Verdict::Exited))
    );
    let (closed, send) = ShellProcess::watched(7);
    drop(send);
    let (mut third, _held) = start(&store);
    assert_eq!(
        third.publish(&key(), later(), &closed).await,
        Err(Refused::Lost(Verdict::Unconfirmed(Unconfirmed::Watch))),
        "a watch closed while still Watching is terminal"
    );
    assert_eq!(store.calls(), 0);
    assert_eq!(store.item(), Item::Absent);
}

#[tokio::test]
async fn a_loss_ready_when_the_readback_finishes_wins_and_settlement_deletes_our_item() {
    let store = Fake::new(Item::Absent);
    let (started, release) = store.hold(Step::Read);
    let shell = shell();
    let loser = shell.clone();
    let thread = release_after(started, release, move || {
        assert!(loser.publish(1));
    });
    let (mut attempt, _held) = start(&store);
    assert_eq!(
        attempt.publish(&key(), later(), &shell).await,
        Err(Refused::Late { cleaned: true })
    );
    thread.join().expect("release thread");
    assert_eq!(
        store.item(),
        Item::Absent,
        "the settlement deleted what the one readback matched"
    );
    assert_eq!(store.calls(), 3, "the add, the one readback and the delete");
    assert_eq!(wiped(&attempt), [Some(true), Some(true)]);
}

#[tokio::test]
async fn a_watch_that_closes_as_the_readback_finishes_settles_instead_of_publishing() {
    let store = Fake::new(Item::Absent);
    let (started, release) = store.hold(Step::Read);
    let (shell, send) = ShellProcess::watched(7);
    let thread = release_after(started, release, move || drop(send));
    let (mut attempt, _held) = start(&store);
    assert_eq!(
        attempt.publish(&key(), later(), &shell).await,
        Err(Refused::Late { cleaned: true })
    );
    thread.join().expect("release thread");
    assert_eq!(store.item(), Item::Absent);
}

fn after(wait: Duration) -> impl FnOnce() + Send + 'static {
    move || std::thread::sleep(wait)
}

#[tokio::test]
async fn a_deadline_during_a_write_settles_by_what_the_write_did() {
    let deadline = || Instant::now() + Duration::from_millis(100);
    let late = Duration::from_millis(300);

    let store = Fake::new(Item::Absent);
    let (started, release) = store.hold(Step::Add);
    let thread = release_after(started, release, after(late));
    let (mut attempt, _held) = start(&store);
    assert_eq!(
        attempt.publish(&key(), deadline(), &shell()).await,
        Err(Refused::Late { cleaned: true }),
        "a late success is ours, so it's deleted"
    );
    thread.join().expect("release thread");
    assert_eq!(store.item(), Item::Absent);
    assert_eq!(wiped(&attempt), [Some(true), Some(true)]);

    let store = Fake::new(Item::Absent);
    store.fail(Step::Add);
    let (started, release) = store.hold(Step::Add);
    let thread = release_after(started, release, after(late));
    let (mut attempt, _held) = start(&store);
    assert_eq!(
        attempt.publish(&key(), deadline(), &shell()).await,
        Err(Refused::Deadline),
        "a late failure needs no cleanup"
    );
    thread.join().expect("release thread");
    assert_eq!(store.calls(), 1, "no readback or delete");

    let store = Fake::new(Item::Absent);
    store.flip_read();
    let (started, release) = store.hold(Step::Add);
    let thread = release_after(started, release, after(late));
    let (mut attempt, _held) = start(&store);
    assert_eq!(
        attempt.publish(&key(), deadline(), &shell()).await,
        Err(Refused::Late { cleaned: false })
    );
    thread.join().expect("release thread");
    assert_eq!(
        store.item(),
        Item::Plain(KEY),
        "a differing item is never deleted"
    );

    let store = Fake::new(Item::Absent);
    store.fail(Step::Delete);
    let (started, release) = store.hold(Step::Add);
    let thread = release_after(started, release, after(late));
    let (mut attempt, _held) = start(&store);
    assert_eq!(
        attempt.publish(&key(), deadline(), &shell()).await,
        Err(Refused::Late { cleaned: false }),
        "a failed late cleanup is recorded"
    );
    thread.join().expect("release thread");
    assert_eq!(store.item(), Item::Plain(KEY));
    assert_eq!(
        attempt.settle().await,
        Stage::Failed,
        "the failed cleanup is kept"
    );
    assert_eq!(attempt.settle().await, Stage::Failed);
    assert_eq!(store.calls(), 3, "the add, the readback and one delete");
}

#[tokio::test]
async fn retirement_deletes_the_published_item_once_and_records_a_failure() {
    let store = Fake::new(Item::Absent);
    let (mut attempt, _held) = start(&store);
    assert_eq!(
        attempt.retire().await,
        Err(Refused::Used),
        "nothing published yet"
    );
    assert_eq!(attempt.publish(&key(), later(), &shell()).await, Ok(()));
    assert_eq!(attempt.retire().await, Ok(()));
    assert_eq!(store.item(), Item::Absent);
    assert_eq!(attempt.retire().await, Err(Refused::Used));

    let store = Fake::new(Item::Absent);
    let (mut attempt, _held) = start(&store);
    assert_eq!(attempt.publish(&key(), later(), &shell()).await, Ok(()));
    store.fail(Step::Delete);
    assert_eq!(
        attempt.retire().await,
        Err(Refused::Store(Step::Delete, Fault::Failed))
    );
    assert_eq!(store.item(), Item::Plain(KEY));
    assert_eq!(attempt.retire().await, Err(Refused::Used));
    assert_eq!(
        attempt.settle().await,
        Stage::Failed,
        "the failed retirement is kept"
    );
    assert_eq!(attempt.settle().await, Stage::Failed);
    assert_eq!(store.calls(), 3, "no delete is retried");
}

#[tokio::test]
async fn a_dropped_publish_keeps_custody_until_settlement_returns() {
    let store = Fake::new(Item::Absent);
    let (started, release) = store.hold(Step::Add);
    let thread = release_after(started, release, after(Duration::from_millis(300)));
    let (mut attempt, held) = start(&store);
    let dropped = tokio::time::timeout(
        Duration::from_millis(50),
        attempt.publish(&key(), later(), &shell()),
    )
    .await;
    assert!(dropped.is_err(), "the publish future was dropped mid-call");
    assert_eq!(held.strong_count(), 1, "custody is held");
    assert_eq!(wiped(&attempt), [None], "the call in flight keeps its copy");
    {
        let settle = attempt.settle();
        tokio::pin!(settle);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut settle)
                .await
                .is_err(),
            "settlement waits for the call"
        );
        assert_eq!(held.strong_count(), 1, "a contender is still busy");
        assert_eq!(settle.await, Stage::Confirmed);
    }
    thread.join().expect("release thread");
    assert_eq!(
        store.item(),
        Item::Absent,
        "the late item was ours and is gone"
    );
    assert_eq!(wiped(&attempt), [Some(true), Some(true)]);
    assert_eq!(held.strong_count(), 1, "custody outlives settlement");
    drop(attempt);
    assert_eq!(held.strong_count(), 0);
}

/// Runs `test` on a runtime with one blocking thread, which the returned sender frees.
fn one_blocking_thread(test: impl AsyncFnOnce(mpsc::Sender<()>)) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .max_blocking_threads(1)
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let (started, running) = mpsc::channel();
        let (free, freed) = mpsc::channel::<()>();
        let blocker = tokio::task::spawn_blocking(move || {
            started.send(()).expect("the test listens");
            freed.recv().expect("the test frees the thread");
        });
        running.recv().expect("the blocker runs");
        test(free).await;
        blocker.await.expect("blocker");
    });
}

#[test]
fn a_forward_call_the_pool_starts_after_a_loss_makes_no_item_call() {
    one_blocking_thread(async |free| {
        let store = Fake::new(Item::Absent);
        let (mut attempt, _held) = start(&store);
        let (shell, key) = (shell(), key());
        let publish = attempt.publish(&key, later(), &shell);
        tokio::pin!(publish);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut publish)
                .await
                .is_err(),
            "the add is queued behind the held thread"
        );
        assert!(shell.publish(1), "the loss lands before any claim");
        free.send(()).expect("the blocker waits");
        assert_eq!(publish.await, Err(Refused::Lost(Verdict::Exited)));
        assert_eq!(store.calls(), 0);
        assert_eq!(store.item(), Item::Absent);
    });
}

#[test]
fn a_forward_call_the_pool_starts_after_the_deadline_makes_no_item_call() {
    one_blocking_thread(async |free| {
        let store = Fake::new(Item::Absent);
        let (mut attempt, _held) = start(&store);
        let (shell, key) = (shell(), key());
        let deadline = Instant::now() + Duration::from_millis(100);
        let publish = attempt.publish(&key, deadline, &shell);
        tokio::pin!(publish);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut publish)
                .await
                .is_err(),
            "the add is queued behind the held thread"
        );
        tokio::time::sleep_until((deadline + Duration::from_millis(20)).into()).await;
        free.send(()).expect("the blocker waits");
        tokio::task::spawn_blocking(|| {})
            .await
            .expect("the one thread ran the queued add first");
        assert_eq!(
            store.calls(),
            0,
            "only the worker's own check ran, and it skipped the add"
        );
        assert_eq!(store.item(), Item::Absent);
        assert_eq!(publish.await, Err(Refused::Deadline));
        assert_eq!(store.calls(), 0);
    });
}

#[tokio::test]
async fn a_publish_dropped_while_its_late_write_is_held_leaves_settle_to_wait() {
    let store = Fake::new(Item::Absent);
    let (started, release) = store.hold(Step::Add);
    let (mut attempt, held) = start(&store);
    let deadline = Instant::now() + Duration::from_millis(50);
    let dropped = tokio::time::timeout(
        Duration::from_millis(150),
        attempt.publish(&key(), deadline, &shell()),
    )
    .await;
    assert!(dropped.is_err(), "dropped while settlement awaits the add");
    started
        .recv_timeout(Duration::from_secs(5))
        .expect("the add started");
    {
        let settle = attempt.settle();
        tokio::pin!(settle);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut settle)
                .await
                .is_err(),
            "settle waits for the held add"
        );
        assert_eq!(held.strong_count(), 1, "a contender is still busy");
        release.send(()).expect("the add waits");
        assert_eq!(settle.await, Stage::Confirmed);
    }
    assert_eq!(
        store.item(),
        Item::Absent,
        "the late add was ours and is gone"
    );
    assert_eq!(store.calls(), 3, "the add, the readback and the delete");
    assert_eq!(held.strong_count(), 1, "custody outlives settlement");
}

#[tokio::test]
async fn a_publish_dropped_while_its_cleanup_delete_is_held_leaves_settle_to_wait() {
    let store = Fake::new(Item::Absent);
    let (added, release_add) = store.hold(Step::Add);
    let (deleting, release_delete) = store.hold(Step::Delete);
    let thread = release_after(added, release_add, after(Duration::from_millis(100)));
    let (mut attempt, held) = start(&store);
    let deadline = Instant::now() + Duration::from_millis(30);
    let dropped = tokio::time::timeout(
        Duration::from_millis(400),
        attempt.publish(&key(), deadline, &shell()),
    )
    .await;
    assert!(dropped.is_err(), "dropped while the cleanup delete is held");
    deleting
        .recv_timeout(Duration::from_secs(5))
        .expect("the cleanup delete started");
    assert_eq!(
        wiped(&attempt),
        [Some(true), Some(true)],
        "the copy and the readback are wiped before the held delete returns"
    );
    {
        let settle = attempt.settle();
        tokio::pin!(settle);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut settle)
                .await
                .is_err(),
            "settle waits for the held delete"
        );
        assert_eq!(held.strong_count(), 1, "a contender is still busy");
        release_delete.send(()).expect("the delete waits");
        assert_eq!(settle.await, Stage::Confirmed);
    }
    thread.join().expect("release thread");
    assert_eq!(store.item(), Item::Absent);
    assert_eq!(store.calls(), 3, "the add, the readback and one delete");
}

#[tokio::test]
async fn a_retire_dropped_while_its_delete_is_held_leaves_settle_to_wait() {
    let store = Fake::new(Item::Absent);
    let (mut attempt, held) = start(&store);
    assert_eq!(attempt.publish(&key(), later(), &shell()).await, Ok(()));
    let (deleting, release) = store.hold(Step::Delete);
    let dropped = tokio::time::timeout(Duration::from_millis(50), attempt.retire()).await;
    assert!(dropped.is_err(), "dropped while the delete is held");
    deleting
        .recv_timeout(Duration::from_secs(5))
        .expect("the delete started");
    assert_eq!(
        format!("{attempt:?}"),
        r#"InstanceAttempt { state: "Retiring", .. }"#
    );
    {
        let settle = attempt.settle();
        tokio::pin!(settle);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut settle)
                .await
                .is_err(),
            "settle waits for the held delete"
        );
        assert_eq!(held.strong_count(), 1, "a contender is still busy");
        release.send(()).expect("the delete waits");
        assert_eq!(settle.await, Stage::Confirmed);
    }
    assert_eq!(store.item(), Item::Absent);
    assert_eq!(attempt.retire().await, Err(Refused::Used));
    assert_eq!(store.calls(), 3, "the add, the readback and one delete");
}

#[tokio::test]
async fn a_loss_during_the_readback_uses_that_readback_and_never_retries_it() {
    for (name, mismatch) in [("a mismatch", true), ("a failure", false)] {
        let store = Fake::new(Item::Absent);
        if mismatch {
            store.flip_next_read();
        } else {
            store.fail_once(Step::Read);
        }
        let (started, release) = store.hold(Step::Read);
        let (shell, verdict) = ShellProcess::watched(7);
        let thread = release_after(started, release, move || {
            verdict.send(Verdict::Exited).expect("the attempt watches");
        });
        let (mut attempt, _held) = start(&store);
        assert_eq!(
            attempt.publish(&key(), later(), &shell).await,
            Err(Refused::Late { cleaned: false }),
            "{name}: the loss beats the readback, and settlement keeps its result"
        );
        thread.join().expect("release thread");
        assert_eq!(store.calls(), 2, "{name}: the add and the one readback");
        assert_eq!(store.item(), Item::Plain(KEY), "{name}: no delete");
        assert_eq!(attempt.settle().await, Stage::Failed, "{name}");
    }
}

#[tokio::test]
async fn the_attempt_prints_no_key_bytes() {
    let store = Fake::new(Item::Absent);
    let (started, release) = store.hold(Step::Add);
    let thread = release_after(started, release, after(Duration::from_millis(100)));
    let (mut attempt, _held) = start(&store);
    let dropped = tokio::time::timeout(
        Duration::from_millis(20),
        attempt.publish(&key(), later(), &shell()),
    )
    .await;
    assert!(dropped.is_err());
    let shown = format!("{attempt:?}");
    assert_eq!(shown, r#"InstanceAttempt { state: "InFlight", .. }"#);
    for planted in ["165", "a5", "A5"] {
        assert!(!shown.contains(planted), "{planted}");
    }
    assert!(!shown.as_bytes().windows(2).any(|pair| pair == [0xA5, 0xA5]));
    assert_eq!(attempt.settle().await, Stage::Confirmed);
    thread.join().expect("release thread");
}

#[tokio::test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
async fn native_a_dropped_publish_keeps_the_session_lock_until_settlement_returns() {
    use crate::platform::{instance_fixture, instance_fixture_clean};
    use crate::startup::custody::CustodyError;

    let support = instance_fixture("dropped");
    let store = Fake::new(Item::Absent);
    let (started, release) = store.hold(Step::Add);
    let thread = release_after(started, release, after(Duration::from_millis(300)));
    let custody = SessionCustody::bootstrap_at(&support).expect("custody");
    let mut attempt = InstanceAttempt::new(custody, Arc::clone(&store));
    let dropped = tokio::time::timeout(
        Duration::from_millis(50),
        attempt.publish(&key(), later(), &shell()),
    )
    .await;
    let busy = SessionCustody::bootstrap_at(&support).map(drop);
    let settled = attempt.settle().await;
    let still = SessionCustody::bootstrap_at(&support).map(drop);
    drop(attempt);
    let free = SessionCustody::bootstrap_at(&support).map(drop);
    thread.join().expect("release thread");
    instance_fixture_clean(&support, &["session.lock"]);
    assert!(dropped.is_err(), "the publish future was dropped mid-call");
    assert_eq!(busy, Err(CustodyError::Busy));
    assert_eq!(settled, Stage::Confirmed);
    assert_eq!(
        still,
        Err(CustodyError::Busy),
        "custody outlives settlement"
    );
    assert_eq!(free, Ok(()));
    assert_eq!(store.item(), Item::Absent);
    assert_eq!(
        store.calls(),
        3,
        "the add, the settlement readback and the delete"
    );
}

/// Test support: each dropped [`Wiped`] buffer's address and whether it held only zeros at the
/// end of its drop, read from the storage itself just before it is freed. Buffers drop on any
/// thread, so the log is shared.
pub(super) mod wipes {
    use std::sync::Mutex;

    use super::super::{Made, Wiped};

    static LOG: Mutex<Vec<(usize, bool)>> = Mutex::new(Vec::new());

    pub(in crate::startup::instance) fn record(bytes: &[u8]) {
        let entry = (bytes.as_ptr().addr(), bytes.iter().all(|&byte| byte == 0));
        LOG.lock().expect("the wipe log").push(entry);
    }

    pub(in crate::startup::instance) fn made(made: &Made, buffer: &Wiped) {
        let since = LOG.lock().expect("the wipe log").len();
        made.lock()
            .expect("the made list")
            .push((buffer.0.as_ptr().addr(), since));
    }

    pub(super) fn of(made: &Made) -> Vec<Option<bool>> {
        let log = LOG.lock().expect("the wipe log");
        made.lock()
            .expect("the made list")
            .iter()
            .map(|&(addr, since)| {
                log[since..]
                    .iter()
                    .find(|(at, _)| *at == addr)
                    .map(|&(_, zero)| zero)
            })
            .collect()
    }
}
