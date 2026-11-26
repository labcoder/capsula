//! Parent-bound starts under real session custody in a fixture Support folder: the opened state
//! runs on custody's loan, every profile file lands in the folder custody locked, and custody
//! outlives the stages until the item settles.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use kumochi_protocol::constants::{
    AUTHORITY_FILE, CONFIG_FILE, DATA_DIR, SESSION_LOCK, STORE_FILE,
};
use kumochi_protocol::message::{ControlToBrain, ControlToShell};

use super::admit_tests::{Item, inherited, parent};
use super::tests::{SHORT, bounded, by};
use crate::ParentBoundError;
use crate::platform::{ShellProcess, instance_fixture, instance_fixture_clean};
use crate::startup::custody::{CustodyError, SessionCustody};
use crate::startup::parent_bound::fixture::{LoseAt, Script, run_on_fixtures};
use crate::test_support::Scratch;

fn fixture() -> ShellProcess {
    ShellProcess::fixture(1, Arc::new(AtomicU8::new(0)))
}

fn entries(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder)
        .expect("list the fixture folder")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

fn sorted(names: &[&str]) -> Vec<String> {
    let mut names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
    names.sort();
    names
}

/// The open takes no lock of its own: with custody's flock held, a second one would be Busy and
/// the open would refuse. The store and the authority land in custody's product folder, and the
/// authority is this open's: the parent is lost once the store opens, so no accounts start.
#[tokio::test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
async fn a_lent_open_takes_no_second_lock_and_makes_its_files_in_the_locked_folder() {
    let support = instance_fixture("lent-open");
    let product = support.join(DATA_DIR);
    let custody = SessionCustody::bootstrap_at(&support).expect("custody");
    let before = entries(&product);
    let scratch = Scratch::new("lent-open");
    let item = Item::default();
    let shell = fixture();
    let (pair, ends) = inherited();
    let script = Script {
        lose: Some(LoseAt::Open),
        ..Script::default()
    };
    let ((ran, stages), ()) = bounded(async {
        tokio::join!(
            run_on_fixtures(&scratch, pair, &shell, by(), item.clone(), custody, script),
            async {
                let mut greeter = parent(ends, &item).await;
                greeter.hellos().await;
                greeter.metrics().await;
                greeter.read_to_end().await;
            },
        )
    })
    .await;
    let (made, beside) = (entries(&product), entries(&support));
    let in_scratch = [STORE_FILE, AUTHORITY_FILE].map(|name| scratch.dir().join(name).exists());
    instance_fixture_clean(
        &support,
        &made.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert_eq!(before, sorted(&[SESSION_LOCK]), "custody made no authority");
    assert_eq!(
        ran,
        Err(ParentBoundError::Incomplete),
        "a store opened and settled unconfirmed"
    );
    assert_eq!(stages, ["init", "open"], "no accounts started");
    assert_eq!(made, sorted(&[SESSION_LOCK, STORE_FILE, AUTHORITY_FILE]));
    assert_eq!(
        beside,
        sorted(&[DATA_DIR]),
        "nothing beside the product folder"
    );
    assert_eq!(
        in_scratch,
        [false, false],
        "nothing in the fixture's scratch"
    );
}

/// Custody's lock outlives the stages' loans: while settlement's item delete is held, after both
/// legs closed, a contender is Busy, and it locks once the delete finishes. The settings come
/// from custody's folder too.
#[tokio::test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
async fn custody_holds_its_lock_through_the_item_delete_after_the_stages_drop() {
    let support = instance_fixture("lent-release");
    let product = support.join(DATA_DIR);
    let custody = SessionCustody::bootstrap_at(&support).expect("custody");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(product.join(CONFIG_FILE))
        .and_then(|mut file| file.write_all(b"assistant_name = \"Loanbot\"\n"))
        .expect("fixture settings");
    let scratch = Scratch::new("lent-release");
    let item = Item::default();
    let (deleting, release) = item.hold_delete();
    let shell = fixture();
    let (pair, ends) = inherited();
    let ((ran, stages), (name, entered, closed, busy)) = bounded(async {
        tokio::join!(
            run_on_fixtures(
                &scratch,
                pair,
                &shell,
                by(),
                item.clone(),
                custody,
                Script::default(),
            ),
            async {
                let mut greeter = parent(ends, &item).await;
                greeter.hellos().await;
                greeter.metrics().await;
                let name = greeter
                    .control_until(|frame| matches!(frame, ControlToShell::Scenes { .. }))
                    .await
                    .into_iter()
                    .find_map(|frame| match frame {
                        ControlToShell::Settings { view, .. } => {
                            Some(view.assistant_name.as_str().to_owned())
                        }
                        _ => None,
                    });
                greeter.send_control(&ControlToBrain::Quitting).await;
                let entered =
                    tokio::task::spawn_blocking(move || deleting.recv_timeout(SHORT * 100))
                        .await
                        .expect("the wait ends");
                let closed = tokio::time::timeout(SHORT * 10, greeter.read_to_end())
                    .await
                    .is_ok();
                let busy = SessionCustody::bootstrap_at(&support).map(drop);
                release.send(()).expect("the delete waits");
                (name, entered, closed, busy)
            },
        )
    })
    .await;
    let after = SessionCustody::bootstrap_at(&support).map(drop);
    instance_fixture_clean(
        &support,
        &[SESSION_LOCK, STORE_FILE, AUTHORITY_FILE, CONFIG_FILE],
    );
    assert_eq!(ran, Ok(()));
    assert_eq!(stages, ["init", "open", "accounts", "containment"]);
    assert_eq!(
        name.as_deref(),
        Some("Loanbot"),
        "settings from custody's folder"
    );
    assert_eq!(entered, Ok(()), "settlement deletes the item");
    assert!(
        closed,
        "both legs closed, after the stages, before the item settled"
    );
    assert_eq!(
        busy,
        Err(CustodyError::Busy),
        "custody holds through the delete"
    );
    assert_eq!(after, Ok(()), "custody lets go once the item settles");
    assert!(item.get().is_none(), "settlement deleted the item");
}
