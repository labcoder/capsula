use std::future::Future as _;
use std::sync::atomic::Ordering;
use std::task::{Context, Waker};

use super::*;
use crate::supervise::fixture;
use crate::voice::stt::Cut;

#[tokio::test]
async fn late_valid_loaded_keeps_the_actual_stt_owner_until_physical_confirmation() {
    late_valid_owner(Cut::Load).await;
}

#[tokio::test]
async fn late_valid_owned_unloaded_keeps_the_actual_stt_owner_until_physical_confirmation() {
    late_valid_owner(Cut::Unload).await;
}

#[expect(
    clippy::too_many_lines,
    reason = "keep the ordered caller, actor, owner and installer proof together"
)]
async fn late_valid_owner(cut: Cut) {
    for confirmed in [false, true] {
        let mut rig = fixture::start::<Speech>();
        let mut exits = rig.handle.exits();
        let budget = Duration::from_secs(1);
        let speech = SpeechHandle {
            inner: rig.handle.clone(),
            deadlines: Deadlines {
                load: budget,
                unload: budget,
                ..DEADLINES
            },
        };
        let (reports, mut observed) = Reports::new();
        let _keep_reports_open = reports.clone();
        let (owner, mut caller) =
            SttHandle::spawn_poll_cut(tokio::runtime::Handle::current(), speech, reports, cut)
                .expect("actual adapter starts");
        let (mut installer, effects) = Installer::start(
            [Durable::Installed, Durable::Absent],
            [true; 2].map(Ok),
            [true; 2],
            &[],
        );
        assert_eq!(
            effects,
            [Effect::Load {
                part: SpeechInput,
                ask: 1
            }]
        );
        if cut == Cut::Load {
            tokio::time::pause();
        }
        owner.load(PathBuf::from("models"), 1);
        let (mut id, request) = rig.next().await;
        assert!(matches!(request, SpeechRequest::Load { .. }));
        if cut == Cut::Unload {
            rig.reply(id, &SpeechReply::Loaded).await;
            let loaded = report(&mut observed).await;
            assert_eq!(
                loaded,
                LoadReport {
                    ask: 1,
                    event: LoadEvent::Loaded
                }
            );
            assert_eq!(installer.reported(SpeechInput, loaded), []);
            tokio::time::pause();
            assert_eq!(
                installer.remove(SpeechInput, Folder::Present),
                (RemoveAnswer::Intent { token: 3 }, vec![])
            );
            assert_eq!(
                installer.intended(SpeechInput, 3, Ok(())),
                [Effect::Unload {
                    part: SpeechInput,
                    ask: 1
                }]
            );
            owner.unload(1);
            let (unload, request) = rig.next().await;
            assert!(matches!(request, SpeechRequest::Unload));
            id = unload;
        }
        let admitted = caller.admitted().await;
        caller.not_ready();
        let terminal = match cut {
            Cut::Load => SpeechReply::Loaded,
            Cut::Unload => SpeechReply::Unloaded,
        };
        rig.reply(id, &terminal).await;
        assert_eq!(caller.ready().await, admitted);
        assert_eq!(Instant::now(), admitted);
        caller.paused();
        caller.owned(1);
        assert_eq!(
            observed.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        );
        assert_eq!(rig.terminations.load(Ordering::Relaxed), 0);
        if cut == Cut::Load {
            assert_eq!(
                installer.remove(SpeechInput, Folder::Present),
                (RemoveAnswer::Intent { token: 3 }, vec![])
            );
            assert_eq!(
                installer.intended(SpeechInput, 3, Ok(())),
                [Effect::Unload {
                    part: SpeechInput,
                    ask: 1
                }]
            );
            owner.unload(1);
        }
        assert_eq!(installer.install(SpeechInput), []);
        tokio::time::advance(budget).await;
        assert_eq!(Instant::now(), admitted + budget);
        caller.paused();
        caller.resume();
        rig.held().await;
        caller.waiting().await;
        caller.owned(1);
        rig.no_more_requests();
        assert_eq!(exits.try_recv(), Err(broadcast::error::TryRecvError::Empty));
        assert_eq!(
            observed.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        );
        assert_eq!(installer.install(SpeechInput), []);
        rig.confirm.send(confirmed).expect("held physical result");
        if confirmed {
            drop(owner);
            caller.stopped().await;
            caller.released();
            let lost = observed.try_recv().expect("owner has finished reporting");
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
        } else {
            caller.stopped().await;
            caller.owned(1);
            let unavailable = observed.try_recv().expect("owner has finished reporting");
            assert_eq!(
                unavailable,
                LoadReport {
                    ask: 1,
                    event: LoadEvent::Unavailable
                }
            );
            assert_eq!(installer.reported(SpeechInput, unavailable), []);
            assert_eq!(
                installer.view().speech_input,
                kumochi_protocol::message::PartState::Unavailable
            );
            owner.unload(1);
            caller.owned(1);
            assert_eq!(installer.install(SpeechInput), []);
            assert_eq!(rig.spawns.load(Ordering::Relaxed), 0);
            drop(owner);
        }
        assert_eq!(
            observed.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        );
        assert_eq!(
            exits.try_recv(),
            Ok(Exit {
                generation: Generation(0),
                confirmed
            })
        );
        assert_eq!(exits.try_recv(), Err(broadcast::error::TryRecvError::Empty));
        drop(rig.handle);
        rig.actor.await.expect("real actor joins");
        assert_eq!(rig.terminations.load(Ordering::Relaxed), 1);
        tokio::time::resume();
    }
}

#[tokio::test]
async fn the_load_wrapper_retires_unusable_replies_without_an_stt_cleanup_request() {
    for (terminal, expected) in [
        (SpeechReply::NotLoaded, SpeechError::NotLoaded),
        (SpeechReply::Unloaded, SpeechError::WorkerLost),
        (
            SpeechReply::Text("unusable".into()),
            SpeechError::WorkerLost,
        ),
    ] {
        let mut rig = fixture::start::<Speech>();
        let speech = SpeechHandle {
            inner: rig.handle.clone(),
            deadlines: DEADLINES,
        };
        let request = speech.load(PathBuf::from("natives"), PathBuf::from("models"));
        let answer = async {
            let (id, request) = rig.next().await;
            assert!(matches!(request, SpeechRequest::Load { .. }));
            rig.reply(id, &terminal).await;
        };
        let (result, ()) = tokio::join!(request, answer);
        assert_eq!(result, Err(expected));
        rig.held().await;
        drop(speech);
        drop(rig.handle);
        rig.confirm.send(true).expect("physical exit");
        rig.actor.await.expect("actor joins");
        assert_eq!(rig.terminations.load(Ordering::Relaxed), 1);
    }
}

#[tokio::test]
async fn invalid_load_terminals_cause_real_retirement_and_one_delete_only_after_physical_confirmation()
 {
    for terminal in [
        SpeechReply::NotLoaded,
        SpeechReply::Unloaded,
        SpeechReply::Text("unusable".into()),
    ] {
        for confirmed in [false, true] {
            let mut rig = fixture::start::<Speech>();
            let speech = SpeechHandle {
                inner: rig.handle.clone(),
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
            let (mut installer, _) = Installer::start(
                [Durable::Installed, Durable::Absent],
                [true; 2].map(Ok),
                [true; 2],
                &[],
            );
            owner.load(PathBuf::from("models"), 1);
            let (id, request) = rig.next().await;
            assert!(matches!(request, SpeechRequest::Load { .. }));
            assert_eq!(
                installer.remove(SpeechInput, Folder::Present),
                (RemoveAnswer::Intent { token: 3 }, vec![])
            );
            assert_eq!(
                installer.intended(SpeechInput, 3, Ok(())),
                [Effect::Unload {
                    part: SpeechInput,
                    ask: 1
                }]
            );
            owner.unload(1);
            rig.reply(id, &terminal).await;
            rig.held().await;
            held(&mut observed).await;
            assert_eq!(installer.install(SpeechInput), []);
            rig.confirm.send(confirmed).expect("physical result");
            if confirmed {
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
            } else {
                let unavailable = report(&mut observed).await;
                assert_eq!(
                    unavailable,
                    LoadReport {
                        ask: 1,
                        event: LoadEvent::Unavailable
                    }
                );
                assert_eq!(installer.reported(SpeechInput, unavailable), []);
                assert_eq!(
                    installer.view().speech_input,
                    kumochi_protocol::message::PartState::Unavailable
                );
                held(&mut observed).await;
                assert_eq!(installer.install(SpeechInput), []);
                assert_eq!(rig.spawns.load(Ordering::Relaxed), 0);
            }
            drop(owner);
            drop(rig.handle);
            tokio::time::timeout(Duration::from_secs(5), rig.actor)
                .await
                .expect("actor shuts down")
                .expect("actor joins");
            assert_eq!(rig.terminations.load(Ordering::Relaxed), 1);
        }
    }
}

#[tokio::test]
async fn rejected_owned_unload_causes_retirement_without_another_request() {
    for (terminal, expected) in [
        (SpeechReply::NotLoaded, SpeechError::NotLoaded),
        (
            SpeechReply::Failed(NativeError::Load),
            SpeechError::Native(NativeError::Load),
        ),
        (SpeechReply::Loaded, SpeechError::WorkerLost),
    ] {
        let mut rig = fixture::start::<Speech>();
        let speech = SpeechHandle {
            inner: rig.handle.clone(),
            deadlines: DEADLINES,
        };
        let request = speech.unload_owned(Generation(0));
        let answer = async {
            let (id, request) = rig.next().await;
            assert!(matches!(request, SpeechRequest::Unload));
            rig.reply(id, &terminal).await;
        };
        let (result, ()) = tokio::join!(request, answer);
        assert_eq!(result, Err(expected));
        rig.held().await;
        let mut exits = rig.handle.exits();
        assert_eq!(exits.try_recv(), Err(broadcast::error::TryRecvError::Empty));
        drop(speech);
        drop(rig.handle);
        rig.confirm.send(true).expect("old owner exits");
        rig.actor.await.expect("actor joins");
        assert_eq!(
            exits.try_recv(),
            Ok(Exit {
                generation: Generation(0),
                confirmed: true
            })
        );
    }
}

#[tokio::test]
async fn queue_refusal_and_cancellation_before_poll_keep_the_known_unload_owner() {
    for fill in [false, true] {
        let mut rig = fixture::start::<Speech>();
        let speech = SpeechHandle {
            inner: rig.handle.clone(),
            deadlines: DEADLINES,
        };
        let mut context = Context::from_waker(Waker::noop());
        if fill {
            for _ in 0..16 {
                let mut request = Box::pin(rig.handle.request(
                    SpeechRequest::Unload,
                    Instant::now() + Duration::from_secs(60),
                ));
                assert!(request.as_mut().poll(&mut context).is_pending());
            }
            assert_eq!(
                speech.unload_owned(Generation(0)).await,
                Err(SpeechError::WorkerLost)
            );
        } else {
            drop(speech.unload_owned(Generation(0)));
        }
        rig.held().await;
        drop(speech);
        drop(rig.handle);
        rig.confirm.send(true).expect("physical exit");
        rig.actor.await.expect("actor joins");
        assert_eq!(rig.terminations.load(Ordering::Relaxed), 1);
    }
}

#[tokio::test]
async fn timely_valid_load_and_owned_unload_do_not_retire_the_worker() {
    let mut rig = fixture::start::<Speech>();
    let speech = SpeechHandle {
        inner: rig.handle.clone(),
        deadlines: DEADLINES,
    };
    let request = speech.load(PathBuf::from("natives"), PathBuf::from("models"));
    let answer = async {
        let (id, request) = rig.next().await;
        assert!(matches!(request, SpeechRequest::Load { .. }));
        rig.reply(id, &SpeechReply::Loaded).await;
    };
    let (loaded, ()) = tokio::join!(request, answer);
    assert_eq!(loaded, Ok(Generation(0)));
    let request = speech.unload_owned(Generation(0));
    let answer = async {
        let (id, request) = rig.next().await;
        assert!(matches!(request, SpeechRequest::Unload));
        rig.reply(id, &SpeechReply::Unloaded).await;
    };
    let (unloaded, ()) = tokio::join!(request, answer);
    assert_eq!(unloaded, Ok(Generation(0)));
    assert_eq!(rig.terminations.load(Ordering::Relaxed), 0);
    let request = speech.load(PathBuf::from("natives"), PathBuf::from("models"));
    let answer = async {
        let (id, request) = rig.next().await;
        assert!(matches!(request, SpeechRequest::Load { .. }));
        rig.reply(id, &SpeechReply::Failed(NativeError::Load)).await;
    };
    let (failed, ()) = tokio::join!(request, answer);
    assert_eq!(failed, Err(SpeechError::Native(NativeError::Load)));
    assert_eq!(rig.terminations.load(Ordering::Relaxed), 0);
    drop(speech);
    drop(rig.handle);
    rig.confirm.send(true).expect("orderly shutdown");
    rig.actor.await.expect("actor joins");
    assert_eq!(rig.terminations.load(Ordering::Relaxed), 1);
}
