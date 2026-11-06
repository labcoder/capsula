use super::*;
use crate::connections::github::{
    GitHubToken,
    api::GitHubUser,
    device::{Account, Removed, Stamp},
    lane::{Job as GitHubJob, Reply},
};
use crate::connections::vault::{Vault, fake::FakeStore};
use crate::test_support::Scratch;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

#[test]
fn an_unopened_vault_refuses_each_github_operation_without_exposing_the_cause() {
    use crate::connections::github::{lane::LoadError, record};
    use crate::connections::{authority::AuthorityError, error::VaultError};
    for why in [
        VaultError::Busy,
        AuthorityError::NewerSchema.into(),
        VaultError::Lock(std::io::Error::other("foreign-error-sentinel")),
    ] {
        let mut why = Some(why);
        let mut worker: Worker<FakeStore> = Worker::new(
            Box::new(move || Err(why.take().unwrap())),
            Arc::new(crate::connections::clock::SystemClock),
        );
        assert!(matches!(
            worker.run(crate::connections::account::Job::Load { patient: false }),
            crate::connections::account::Done::Loaded(Err(_))
        ));
        let loaded = worker.github(GitHubJob::Load {
            stamp: Stamp { generation: 3 },
        });
        assert_eq!(format!("{loaded:?}"), "GitHubCredentialReply");
        assert!(matches!(
            loaded,
            Reply::Loaded {
                stamp: Stamp { generation: 3 },
                result: Err(LoadError::Unavailable)
            }
        ));
        let bytes = Secret::new(Zeroizing::new(
            br#"{"token":"ghu_fake_marker","login":"sentinel","id":7}"#.to_vec(),
        ));
        let job = GitHubJob::Save {
            stamp: Stamp { generation: 3 },
            fence: Arc::new(AtomicU64::new(3)),
            account: record::decode(&bytes).unwrap(),
        };
        let (reply, _receiver) = oneshot::channel();
        let work = Work::GitHub { job, reply };
        let debug = format!("{work:?}");
        for marker in ["ghu_fake_marker", "sentinel", "foreign-error-sentinel"] {
            assert!(!debug.contains(marker));
            assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
        }
        let Work::GitHub { job, .. } = work else {
            unreachable!()
        };
        assert!(matches!(
            worker.github(job),
            Reply::Saved {
                stamp: Stamp { generation: 3 },
                saved: false
            }
        ));
        assert!(matches!(
            worker.github(GitHubJob::Remove { generation: 4 }),
            Reply::Removed {
                generation: 4,
                result: Removed::NotDurable
            }
        ));
    }
}

#[tokio::test]
async fn a_closed_save_receiver_cannot_drop_the_queued_removal() {
    let scratch = Scratch::new("github-worker-order");
    let path = scratch.db().with_file_name("credential-test.lock");
    let store = FakeStore::default();
    let opened_store = store.clone();
    let (ready_tx, ready_rx) = std_mpsc::channel();
    let worker = Worker::new(
        Box::new(move || {
            ready_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("release startup");
            Vault::with_service(opened_store.clone(), "github-worker-test", &path)
        }),
        Arc::new(crate::connections::clock::SystemClock),
    );
    let (loaded_tx, loaded_rx) = std_mpsc::channel();
    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let (reads_tx, _reads_rx) = mpsc::unbounded_channel();
    let (jobs, released) = spawn(worker, loaded_tx, events_tx, reads_tx).unwrap();
    jobs.send(Work::Account(crate::connections::account::Job::Load {
        patient: false,
    }))
    .unwrap();
    let (save_tx, save_rx) = oneshot::channel();
    drop(save_rx);
    jobs.send(Work::GitHub {
        job: GitHubJob::Save {
            stamp: Stamp { generation: 1 },
            fence: Arc::new(AtomicU64::new(1)),
            account: Account {
                token: Secret::new(
                    GitHubToken::new(Zeroizing::new("ghu_fake_worker_sentinel".to_owned()))
                        .unwrap(),
                ),
                user: GitHubUser {
                    login: kumochi_protocol::message::GitHubLogin::new("octocat").unwrap(),
                    id: 7,
                },
            },
        },
        reply: save_tx,
    })
    .unwrap();
    let (remove_tx, remove_rx) = oneshot::channel();
    jobs.send(Work::GitHub {
        job: GitHubJob::Remove { generation: 2 },
        reply: remove_tx,
    })
    .unwrap();
    ready_tx.send(()).unwrap();
    let (loaded, key) = loaded_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("startup finished");
    assert!(matches!(
        loaded,
        Input::Vault(crate::connections::account::Done::Loaded(Ok(_)))
    ));
    assert!(matches!(key, Ok(None)));
    let removed = tokio::time::timeout(Duration::from_secs(5), remove_rx)
        .await
        .expect("removal finishes")
        .expect("removal reply");
    assert!(matches!(
        removed,
        Reply::Removed {
            generation: 2,
            result: Removed::Clean
        }
    ));
    drop(jobs);
    tokio::time::timeout(Duration::from_secs(5), released)
        .await
        .expect("worker exits")
        .unwrap();
    assert_eq!(store.names().len(), 1);
    assert!(store.names()[0].starts_with("github-worker-test/gen/host/"));
    let conn = rusqlite::Connection::open(
        scratch
            .db()
            .with_file_name(crate::connections::authority::FILE),
    )
    .unwrap();
    let active: i64 = conn
        .query_row(
            "SELECT count(*) FROM generations WHERE kind='github' AND state='active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(active, 0);
    let removed: i64 = conn
        .query_row(
            "SELECT count(*) FROM generations WHERE kind='github' AND state='removed'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(removed, 1);
}
