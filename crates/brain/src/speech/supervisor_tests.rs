use std::time::Duration;

use kumochi_protocol::voice::VoicePart::SpeechInput;
use tokio::sync::mpsc::UnboundedReceiver;

use super::*;
use crate::supervise::WorkerLost;
use crate::voice::installer::{Durable, Effect, Folder, Installer, RemoveAnswer};
use crate::voice::stt::SttHandle;
use crate::voice::{LoadEvent, LoadReport, Reports};

#[path = "ownership_tests.rs"]
mod ownership;

struct End(broadcast::Sender<Exit>);

impl Drop for End {
    fn drop(&mut self) {
        self.0
            .send(Exit {
                generation: Generation(0),
                confirmed: false,
            })
            .ok();
    }
}

async fn next<T>(requests: &mut tokio::sync::mpsc::Receiver<T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), requests.recv())
        .await
        .expect("request progress")
        .expect("request")
}

async fn report(reports: &mut UnboundedReceiver<LoadReport>) -> LoadReport {
    tokio::time::timeout(Duration::from_secs(5), reports.recv())
        .await
        .expect("owner progress")
        .expect("owner report")
}

async fn held(reports: &mut UnboundedReceiver<LoadReport>) {
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(100), reports.recv())
            .await
            .map_err(|_| ()),
        Err(()),
        "no terminal owner report before release"
    );
}

#[tokio::test]
async fn a_failed_load_request_cannot_delete_or_reinstall_until_its_worker_exit_is_confirmed() {
    let (inner, mut requests) = supervise::detached::<Speech>();
    let exits = inner.test_exits();
    let _end = End(exits.clone());
    let speech = SpeechHandle {
        inner,
        deadlines: DEADLINES,
    };
    let (reports, mut observed) = Reports::new();
    let owner = SttHandle::spawn(
        tokio::runtime::Handle::current(),
        speech,
        PathBuf::from("natives"),
        reports,
    )
    .expect("owner starts");
    let (mut installer, effects) = Installer::start(
        [Durable::Installed, Durable::Absent],
        [true; 2].map(Ok),
        [true; 2],
        &[],
        true,
    );
    assert_eq!(
        effects,
        [Effect::Load {
            part: SpeechInput,
            ask: 1
        }]
    );
    owner.load(PathBuf::from("models"), 1);
    let (request, answer) = next(&mut requests).await;
    assert!(matches!(request, SpeechRequest::Load { .. }));
    assert_eq!(
        installer.remove(SpeechInput, Folder::Present),
        (RemoveAnswer::Intent { token: 3 }, Vec::new())
    );
    assert_eq!(
        installer.intended(SpeechInput, 3, Ok(())),
        [Effect::Unload {
            part: SpeechInput,
            ask: 1
        }]
    );
    owner.unload(1);
    answer
        .send(Err(WorkerLost::Lost))
        .expect("load request fails");
    held(&mut observed).await;
    assert_eq!(installer.install(SpeechInput), []);
    exits
        .send(Exit {
            generation: Generation(1),
            confirmed: true,
        })
        .expect("subscribed before load");
    held(&mut observed).await;
    exits
        .send(Exit {
            generation: Generation(0),
            confirmed: true,
        })
        .expect("matching worker exits");
    let lost = report(&mut observed).await;
    assert_eq!(
        lost,
        LoadReport {
            ask: 1,
            event: LoadEvent::Lost
        }
    );
    assert_eq!(
        installer.reported(SpeechInput, lost),
        [Effect::Delete {
            part: SpeechInput,
            token: 3
        }]
    );
    assert_eq!(installer.reported(SpeechInput, lost), []);
}

/// A supervisor made fatal without a new exit, as when an idle worker's exit goes unconfirmed or a
/// replacement's threads don't start, refuses each later load before dispatch: speech input
/// reports itself unavailable, not lost.
#[tokio::test]
async fn a_load_refused_by_a_fatal_supervisor_reports_speech_input_unavailable() {
    let (inner, _requests) = supervise::detached::<Speech>();
    inner.test_fatal();
    let speech = SpeechHandle {
        inner,
        deadlines: DEADLINES,
    };
    let (reports, mut observed) = Reports::new();
    let owner = SttHandle::spawn(
        tokio::runtime::Handle::current(),
        speech,
        PathBuf::from("natives"),
        reports,
    )
    .expect("owner starts");
    owner.load(PathBuf::from("models"), 1);
    assert_eq!(
        report(&mut observed).await,
        LoadReport {
            ask: 1,
            event: LoadEvent::Unavailable
        }
    );
}

#[tokio::test]
async fn an_unload_reply_from_another_generation_does_not_release_the_loaded_model() {
    let (inner, mut requests) = supervise::detached::<Speech>();
    let exits = inner.test_exits();
    let _end = End(exits.clone());
    let speech = SpeechHandle {
        inner,
        deadlines: DEADLINES,
    };
    let (reports, mut observed) = Reports::new();
    let owner = SttHandle::spawn(
        tokio::runtime::Handle::current(),
        speech,
        PathBuf::from("natives"),
        reports,
    )
    .expect("owner starts");
    owner.load(PathBuf::from("models"), 1);
    let (_, answer) = next(&mut requests).await;
    answer
        .send(Ok((SpeechReply::Loaded, Generation(0))))
        .expect("loaded");
    assert_eq!(
        report(&mut observed).await,
        LoadReport {
            ask: 1,
            event: LoadEvent::Loaded
        }
    );
    owner.unload(1);
    let (request, answer) = next(&mut requests).await;
    assert!(matches!(request, SpeechRequest::Unload));
    answer
        .send(Ok((SpeechReply::Unloaded, Generation(1))))
        .expect("different worker answered");
    held(&mut observed).await;
    exits
        .send(Exit {
            generation: Generation(0),
            confirmed: true,
        })
        .expect("old model worker exits");
    assert_eq!(
        report(&mut observed).await,
        LoadReport {
            ask: 1,
            event: LoadEvent::Lost
        }
    );
}

#[tokio::test]
async fn a_matching_unload_reply_releases_without_waiting_for_process_exit() {
    let (inner, mut requests) = supervise::detached::<Speech>();
    let _end = End(inner.test_exits());
    let speech = SpeechHandle {
        inner,
        deadlines: DEADLINES,
    };
    let (reports, mut observed) = Reports::new();
    let owner = SttHandle::spawn(
        tokio::runtime::Handle::current(),
        speech,
        PathBuf::from("natives"),
        reports,
    )
    .expect("owner starts");
    owner.load(PathBuf::from("models"), 1);
    let (_, answer) = next(&mut requests).await;
    answer
        .send(Ok((SpeechReply::Loaded, Generation(0))))
        .expect("loaded");
    assert_eq!(
        report(&mut observed).await,
        LoadReport {
            ask: 1,
            event: LoadEvent::Loaded
        }
    );
    owner.unload(1);
    let (request, answer) = next(&mut requests).await;
    assert!(matches!(request, SpeechRequest::Unload));
    answer
        .send(Ok((SpeechReply::Unloaded, Generation(0))))
        .expect("same worker released model");
    assert_eq!(
        report(&mut observed).await,
        LoadReport {
            ask: 1,
            event: LoadEvent::Unloaded
        }
    );
}

#[tokio::test]
async fn a_native_failed_load_reply_releases_without_waiting_for_process_exit() {
    let (inner, mut requests) = supervise::detached::<Speech>();
    let _end = End(inner.test_exits());
    let speech = SpeechHandle {
        inner,
        deadlines: DEADLINES,
    };
    let (reports, mut observed) = Reports::new();
    let owner = SttHandle::spawn(
        tokio::runtime::Handle::current(),
        speech,
        PathBuf::from("natives"),
        reports,
    )
    .expect("owner starts");
    owner.load(PathBuf::from("models"), 1);
    let (_, answer) = next(&mut requests).await;
    answer
        .send(Ok((
            SpeechReply::Failed(NativeError::Create),
            Generation(0),
        )))
        .expect("resources freed");
    assert_eq!(
        report(&mut observed).await,
        LoadReport {
            ask: 1,
            event: LoadEvent::Failed
        }
    );
}
