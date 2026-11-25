use std::{
    cell::Cell,
    fs,
    time::{Duration, Instant},
};

use anyhow::anyhow;
use kumochi_protocol::message::{
    ConnectionKind, ControlToShell, DataToShell, Effort, EffortSet, ModelInfo, ModelName,
    ModelSlug, Phase, RunId, RunOrigin, ThreadId,
};

use serde_json::{Value, json};

use super::super::{
    startup,
    tests::{PAIR_FIXTURE, SAFE_PLAN, TEXT_ONLY, clean_metadata},
};
use super::{
    CLAIM, Fact, Frame, Outcomes, PAIR, PLAN, Sample, Seen, claim_checked, confirm_setting, drive,
    join_turn, ledger, projector, readiness, report,
};

#[test]
fn the_catalog_projects_exact_low_for_exact_slugs_and_retains_no_content() {
    let model = |slug, supported_efforts| ModelInfo {
        slug: ModelSlug::new(slug).expect("catalog slug"),
        display_name: ModelName::new("PRIVATE_FIXTURE").expect("catalog label"),
        supported_efforts,
    };
    let mut project = projector();
    let catalog = |models| {
        Frame::Data(
            DataToShell::Models {
                connection: ConnectionKind::ChatGpt,
                models,
            },
            500,
        )
    };
    let rows = vec![
        model("gpt-6-luna", None),
        model("gpt-6.1-sol", EffortSet::new([Effort::High])),
        model("gpt-6.1-sol-other", EffortSet::new([Effort::Low])),
    ];
    assert_eq!(
        project(catalog(rows)),
        Ok(Some(Fact::Catalog([false, false])))
    );
    let rows = vec![model(
        "gpt-6.1-sol",
        EffortSet::new([Effort::High, Effort::Low]),
    )];
    let projected = project(catalog(rows));
    assert_eq!(projected, Ok(Some(Fact::Catalog([false, true]))));
    assert!(!format!("{projected:?}").contains("PRIVATE_FIXTURE"));
}

#[test]
fn readiness_waits_for_repaired_settings_and_refusal_cannot_recover() {
    let main = ThreadId(9);
    let saved = (1, Effort::High);
    let pending = [
        Fact::Setting(main, None, false),
        Fact::Ready(4),
        Fact::Catalog([true, false]),
        Fact::Ready(1),
        Fact::Ready(2),
    ];
    let mut ready = pending
        .into_iter()
        .chain([Fact::Setting(main, Some(saved), false)]);
    let read = || ready.next().ok_or_else(|| anyhow!("bounded fixture ended"));
    assert_eq!(
        readiness(read).expect("repaired"),
        (main, Some(saved), [true, false])
    );

    let mut refused = [Fact::Refused]
        .into_iter()
        .chain(pending)
        .chain([Fact::Setting(main, Some(saved), false)]);
    let read = || {
        refused
            .next()
            .ok_or_else(|| anyhow!("bounded fixture ended"))
    };
    assert_eq!(
        readiness(read).expect_err("not ready").to_string(),
        "disposable profile not ready"
    );
}

#[test]
fn a_setting_requires_a_fresh_exact_confirmation_and_refusal_sticks() {
    let main = ThreadId(9);
    let wanted = (0, Effort::Low);
    let current = (1, Effort::High);
    let sent = Instant::now();
    let ack = Fact::Setting(main, Some(wanted), true);
    let mut responses = [Seen {
        at: sent,
        frame: ack,
    }]
    .into_iter();
    let read = || responses.next().ok_or_else(|| anyhow!("fixture EOF"));
    assert_eq!(
        confirm_setting(main, current, wanted, sent, &mut [true; 2], read).expect("setting reply"),
        Some(wanted)
    );
    for invalid in [
        vec![
            Seen {
                at: sent,
                frame: Fact::Refused,
            },
            Seen {
                at: sent,
                frame: ack,
            },
        ],
        vec![Seen {
            at: sent
                .checked_sub(Duration::from_micros(1))
                .expect("stale fixture"),
            frame: ack,
        }],
        vec![Seen {
            at: sent,
            frame: Fact::Setting(main, Some(wanted), false),
        }],
        vec![Seen {
            at: sent,
            frame: Fact::Setting(ThreadId(5), Some(wanted), true),
        }],
        vec![Seen {
            at: sent,
            frame: Fact::Setting(main, Some((1, Effort::Low)), true),
        }],
    ] {
        let mut scripted = invalid.into_iter();
        let read = || scripted.next().ok_or_else(|| anyhow!("fixture EOF"));
        assert_eq!(
            confirm_setting(main, current, wanted, sent, &mut [true; 2], read)
                .expect("not a pipe loss"),
            None
        );
    }
}

#[test]
fn data_can_precede_control_but_cannot_complete_the_wrong_turn() {
    let sent = Instant::now();
    let run = RunId(42);
    let main = ThreadId(4);
    let join = |frames: Vec<Fact>| {
        let mut frames = frames.into_iter().enumerate();
        join_turn(main, 1, 0, sent, &mut [true; 2], || {
            let (n, frame) = frames.next().ok_or_else(|| anyhow!("fixture EOF"))?;
            Ok(Seen {
                at: sent + Duration::from_micros(12 + u64::try_from(n)?),
                frame,
            })
        })
    };
    let tail = [
        Fact::End(run),
        Fact::Named(1, run, main),
        Fact::Terminal(run, main, true),
    ];
    let observed = join(
        [
            Fact::Text(run),
            Fact::Ready(1),
            Fact::Catalog([true, false]),
        ]
        .into_iter()
        .chain(tail)
        .collect(),
    )
    .expect("exact typed turn")
    .expect("not refused");
    assert_eq!(
        (observed.run, observed.target, observed.pipe_us),
        (run, 0, 12)
    );
    for bad in [
        vec![Fact::Text(RunId(41)), tail[0], tail[1], tail[2]],
        vec![Fact::Text(run), tail[0], Fact::Named(2, run, main), tail[2]],
        vec![
            Fact::Text(run),
            tail[0],
            tail[1],
            Fact::Terminal(run, ThreadId(5), true),
        ],
        vec![tail[0], tail[1], tail[2]],
        vec![tail[0], Fact::Text(run), tail[1], tail[2]],
        vec![Fact::Text(run), tail[1], tail[2]],
    ] {
        assert!(join(bad).is_err());
    }
}

#[test]
fn the_projection_discards_empty_text_but_refuses_even_empty_late_data() {
    let mut project = projector();
    let run = RunId(42);
    let text = |delta: &str| {
        Frame::Data(
            DataToShell::AssistantText {
                run,
                delta: delta.into(),
            },
            34,
        )
    };
    assert_eq!(project(text("")), Ok(None));
    let projected = project(text("PRIVATE_FIXTURE"));
    assert_eq!(projected, Ok(Some(Fact::Text(run))));
    assert!(!format!("{projected:?}").contains("PRIVATE_FIXTURE"));
    assert_eq!(
        project(Frame::Data(DataToShell::RunEnd { run }, 15)),
        Ok(Some(Fact::End(run)))
    );
    assert_eq!(project(text("")), Err(()));
}

#[test]
fn the_ledger_requires_complete_owned_records_and_never_reports_partial_samples() {
    let encoded = "ready 7 1 10\nreserve 7 1 42 1 6 1 1 1 16\ntext 7 1 19\nclosed 7 1 20 21\naux 7 1 5 24\nfinal 7 1 1 29\n";
    let exact = ledger::parse(encoded.as_bytes(), 7, 1).expect("one complete reservation");
    assert!(exact.finished);
    assert_eq!(exact.auxiliary, [0, 0, 0, 0, 1]);
    assert_eq!(exact.attempts[0].run, RunId(42));
    assert_eq!(exact.attempts[0].text, Some(19));
    for invalid in [
        encoded.trim_end().to_owned(),
        encoded.replace("ready 7 1", "ready 7 2"),
        encoded.replace("closed 7", "closed 8"),
        encoded.replace("closed 7 1 20", "closed 7 2 20"),
        encoded.replace("closed 7 1 20", "closed 7 1 22"),
        encoded.replace("final 7 1 1", "final 7 0 1"),
        encoded.replace("aux 7 1 5", "aux 7 1 0"),
        encoded.replace("text 7 1 19", "text 7 1 PRIVATE_FIXTURE"),
    ] {
        let error = ledger::parse(invalid.as_bytes(), 7, 1)
            .err()
            .expect("invalid receipt");
        assert!(!format!("{error:#}").contains("PRIVATE_FIXTURE"));
    }
    let partial = [Sample {
        run: RunId(42),
        target: 0,
        pipe_us: 12,
    }];
    assert_eq!(
        report(&partial, &exact, &Outcomes::default())
            .expect_err("not ten samples")
            .to_string(),
        "incomplete target"
    );
    let empty = ledger::parse(b"ready 7 1 10\nfinal 7 0 0 11\n", 7, 1).expect("no reservations");
    assert_eq!(
        report(&[], &empty, &Outcomes::default()).expect("two not-run targets"),
        concat!(
            r#"{"status":"completed","model_charge":0,"auxiliary":[0, 0, 0, 0, 0],"targets":["#,
            r#"{"status":"not-run","completed":0},{"status":"not-run","completed":0}],"work":"not-started","restore":"not-needed","exit":"not-started"}"#
        )
    );
}

#[test]
fn a_new_connection_or_pending_work_is_refused_before_claim_or_spawn() {
    for (scope, sql, reason) in [
        (
            true,
            "INSERT INTO generations VALUES ('anthropic','active');",
            "connection kinds or pending authority work are not approved",
        ),
        (
            true,
            "INSERT INTO generations VALUES ('chatgpt','pending');",
            "connection kinds or pending authority work are not approved",
        ),
        (
            true,
            "UPDATE control SET marker = json_set(marker, '$.request_pending', json('true'));",
            "connection kinds or pending authority work are not approved",
        ),
        (
            true,
            "UPDATE control SET marker = json_set(marker, '$.sign_out', json('{}'));",
            "connection kinds or pending authority work are not approved",
        ),
        (
            false,
            "INSERT INTO actions VALUES ('{\"kind\":\"posting\"}');",
            "the profile has history or a pending store action",
        ),
    ] {
        let (db, authority) = clean_metadata();
        let folder = crate::scratch::dir("first-text-metadata");
        let plan = startup::check(&db, &authority, TEXT_ONLY, 19).expect("approved startup");
        assert_eq!(plan, SAFE_PLAN);
        fs::write(folder.join(PLAN), plan).expect("approved plan");
        fs::write(folder.join(PAIR), PAIR_FIXTURE).expect("approved pair");
        (if scope { &authority } else { &db })
            .execute_batch(sql)
            .expect("changed metadata");
        let actual = claim_checked(
            &folder,
            PAIR_FIXTURE,
            || startup::check(&db, &authority, TEXT_ONLY, 19),
            || -> anyhow::Result<()> { panic!("unapproved startup must never reach the child") },
        );
        assert_eq!(
            actual.expect_err("before anything starts").to_string(),
            reason
        );
        assert!(!folder.join(CLAIM).exists());
    }
}

#[test]
fn text_only_settings_are_checked_before_taking_the_claim() {
    let (db, authority) = clean_metadata();
    let folder = crate::scratch::dir("first-text-settings");
    fs::write(folder.join(PLAN), SAFE_PLAN).expect("approved plan");
    fs::write(folder.join(PAIR), PAIR_FIXTURE).expect("approved pair");
    let changed = TEXT_ONLY.replace("hold_to_talk=false", "hold_to_talk=true");
    let result = claim_checked(
        &folder,
        PAIR_FIXTURE,
        || startup::check(&db, &authority, &changed, 19),
        || -> anyhow::Result<()> { panic!("voice settings must never reach the child") },
    );
    assert_eq!(
        result.expect_err("not text-only").to_string(),
        "settings are not text-only"
    );
    assert!(!folder.join(CLAIM).exists());
}

#[test]
fn another_pair_is_refused_before_inspection_claim_or_spawn() {
    let folder = crate::scratch::dir("first-text-other-pair");
    fs::write(folder.join(PLAN), SAFE_PLAN).expect("approved plan");
    fs::write(folder.join(PAIR), PAIR_FIXTURE).expect("approved pair");
    let mut other = PAIR_FIXTURE;
    other[23] ^= 1;
    let result = claim_checked(
        &folder,
        other,
        || -> anyhow::Result<String> { panic!("another profile must never be inspected") },
        || -> anyhow::Result<()> { panic!("another profile must never reach the child") },
    );
    assert_eq!(
        result.expect_err("another pair").to_string(),
        "the profile folders differ from the prepared ones"
    );
    assert!(!folder.join(CLAIM).exists());
}

#[test]
fn a_missing_receipt_keeps_the_only_claim_and_a_repeat_cannot_report_another_charge() {
    let folder = crate::scratch::dir("first-text-charge");
    fs::write(folder.join(PLAN), SAFE_PLAN).expect("approved plan");
    fs::write(folder.join(PAIR), PAIR_FIXTURE).expect("approved pair");
    let mismatched = claim_checked(
        &folder,
        PAIR_FIXTURE,
        || Ok(SAFE_PLAN.replace("connections=chatgpt", "connections=chatgpt,anthropic")),
        || -> anyhow::Result<()> { panic!("a changed plan cannot start its child") },
    );
    assert_eq!(
        mismatched.expect_err("plan changed").to_string(),
        "budget or startup work differs from the prepared plan"
    );
    assert!(!folder.join(CLAIM).exists());
    let result = claim_checked(
        &folder,
        PAIR_FIXTURE,
        || Ok(SAFE_PLAN.to_owned()),
        || -> anyhow::Result<()> {
            assert!(folder.join(CLAIM).exists());
            Err(anyhow!("fixture exited before a receipt"))
        },
    );
    assert_eq!(
        result.expect_err("no receipt").to_string(),
        "fixture exited before a receipt"
    );
    assert_eq!(
        fs::read_to_string(folder.join(PLAN)).expect("durable amount"),
        SAFE_PLAN
    );
    let duplicate = claim_checked(
        &folder,
        PAIR_FIXTURE,
        || Ok(SAFE_PLAN.to_owned()),
        || -> anyhow::Result<()> {
            panic!("a duplicate cannot start the charge-reporting callback")
        },
    );
    assert_eq!(
        duplicate.expect_err("already charged").to_string(),
        "run folder was already claimed"
    );
}

#[test]
fn failed_or_refused_typed_work_restores_the_saved_choice_then_quits_without_a_next_turn() {
    let main = ThreadId(7);
    let run = RunId(42);
    let sent = Instant::now();
    let saved = (1, Effort::High);
    for failed in [true, false] {
        let mut project = projector();
        let frames = if failed {
            let state = |phase, origin| {
                Frame::Control(ControlToShell::RunState {
                    run,
                    thread: main,
                    phase,
                    origin,
                    error: None,
                })
            };
            vec![
                state(Phase::Thinking, Some(RunOrigin::Typed { turn: 1 })),
                state(Phase::Failed, None),
                Frame::Data(DataToShell::RunEnd { run }, 12),
            ]
        } else {
            vec![Frame::Control(ControlToShell::TurnRefused {
                turn: 1,
                thread: Some(main),
            })]
        };
        let mut facts = frames.into_iter().map(|frame| {
            project(frame)
                .expect("ordinary failure is not a broken pipe")
                .expect("typed outcome")
        });
        let actual_choice = Cell::new(saved);
        let submitted = Cell::new(0);
        let mut outcomes = Outcomes::default();
        let result = drive(
            Ok((main, Some(saved), [true, false])),
            19,
            sent,
            &mut outcomes,
            |thread, current, choice, _, eligible| {
                let reply = settings_reply(
                    thread,
                    current,
                    choice,
                    sent,
                    eligible,
                    [Fact::Setting(thread, Some(choice), true)],
                )?;
                actual_choice.set(choice);
                Ok(reply)
            },
            |thread, turn, target, eligible| {
                assert_eq!(
                    (thread, turn, target),
                    (main, 1, 0),
                    "no next submit after an ordinary failure"
                );
                assert_eq!(actual_choice.get(), (0, Effort::Low));
                submitted.set(submitted.get() + 1);
                join_turn(main, turn, target, sent, eligible, || {
                    facts
                        .next()
                        .map(|frame| Seen { at: sent, frame })
                        .ok_or_else(|| anyhow!("fixture EOF"))
                })
            },
            || {
                assert_eq!(
                    actual_choice.get(),
                    saved,
                    "quit is after the exact restoration"
                );
                assert_eq!(submitted.get(), 1);
                Ok(())
            },
        );
        assert_eq!(
            result.err().expect("full-cap failure").to_string(),
            "typed turn failed or was refused"
        );
        assert!(
            facts.next().is_none(),
            "the failed run's data end was consumed"
        );
        assert_eq!(
            outcomes.invalid_receipt(19),
            r#"{"status":"invalid","model_charge":19,"auxiliary":null,"work":"failed","restore":"confirmed","exit":"confirmed"}"#
        );
    }
}

#[test]
fn a_quit_error_does_not_erase_an_unconfirmed_restore() {
    let main = ThreadId(7);
    let saved = (1, Effort::High);
    let mut outcomes = Outcomes {
        exit: "unconfirmed",
        ..Outcomes::default()
    };
    let result = drive(
        Ok((main, Some(saved), [true, false])),
        19,
        Instant::now(),
        &mut outcomes,
        |_, _, choice, _, _| Ok(if choice == saved { None } else { Some(choice) }),
        |_, _, _, _| Ok(None),
        || Err(anyhow!("fixture quit failed")),
    );
    assert!(result.is_err());
    assert_eq!(
        outcomes.invalid_receipt(19),
        r#"{"status":"invalid","model_charge":19,"auxiliary":null,"work":"failed","restore":"unconfirmed","exit":"unconfirmed"}"#
    );
}

#[test]
fn pipe_loss_leaves_restoration_unknown_instead_of_trying_another_setting() {
    let main = ThreadId(7);
    let saved = (1, Effort::High);
    let mut outcomes = Outcomes {
        exit: "unconfirmed",
        ..Outcomes::default()
    };
    let mut selected = false;
    let mut cleanup = false;
    let result = drive(
        Ok((main, Some(saved), [true, false])),
        19,
        Instant::now(),
        &mut outcomes,
        |_, _, choice, _, _| {
            assert!(
                !std::mem::replace(&mut selected, true),
                "no further settings write"
            );
            assert_eq!(
                choice,
                (0, Effort::Low),
                "a lost pipe cannot confirm a restore"
            );
            Ok(Some(choice))
        },
        |_, _, _, _| Err(anyhow!("fixture pipe ended")),
        || {
            cleanup = true;
            Err(anyhow!("fixture forced exit"))
        },
    );
    assert!(result.is_err());
    assert!(
        selected,
        "the fixture selected a different choice before pipe loss"
    );
    assert!(
        cleanup,
        "the owned-child cleanup still runs after pipe loss"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&outcomes.invalid_receipt(19)).expect("JSON receipt"),
        json!({
            "status": "invalid", "model_charge": 19, "auxiliary": null,
            "work": "failed", "restore": "unknown", "exit": "unconfirmed"
        })
    );
}

#[test]
fn consecutive_completed_attempts_require_increasing_build_keys() {
    let fixture = "ready 7 2 10\nreserve 7 1 42 1 6 1 1 1 16\ntext 7 1 19\npooled 7 1 21\nreserve 7 2 44 1 9 1 1 1 24\ntext 7 2 26\npooled 7 2 28\nfinal 7 2 0 29\n";
    let observed =
        ledger::parse(fixture.as_bytes(), 7, 2).expect("a gap between build keys is valid");
    assert_eq!(observed.attempts.len(), 2);
    assert!(observed.finished);
    for key in [5, 6] {
        let invalid = fixture.replace("44 1 9", &format!("44 1 {key}"));
        assert!(ledger::parse(invalid.as_bytes(), 7, 2).is_err());
    }
}

fn settings_reply(
    main: ThreadId,
    current: super::Choice,
    wanted: super::Choice,
    sent: Instant,
    eligible: &mut [bool; 2],
    frames: impl IntoIterator<Item = Fact>,
) -> anyhow::Result<Option<super::Choice>> {
    let mut frames = frames.into_iter();
    confirm_setting(main, current, wanted, sent, eligible, || {
        frames
            .next()
            .map(|frame| Seen { at: sent, frame })
            .ok_or_else(|| anyhow!("fixture EOF"))
    })
}

#[test]
fn a_disqualifying_update_before_an_exact_setting_ack_prevents_submits() {
    let main = ThreadId(7);
    let sent = Instant::now();
    let saved = (1, Effort::High);
    let low = (0, Effort::Low);
    for update in [Fact::Unavailable, Fact::Catalog([false, true])] {
        let selected = Cell::new(saved);
        let writes = Cell::new(0);
        let mut outcomes = Outcomes::default();
        let result = drive(
            Ok((main, Some(saved), [true, false])),
            10,
            sent,
            &mut outcomes,
            |thread, current, wanted, _, eligible| {
                writes.set(writes.get() + 1);
                assert_eq!(thread, main);
                assert_eq!(current, selected.get());
                let frames = if wanted == low {
                    [
                        update,
                        Fact::Ready(1),
                        Fact::Catalog([true, true]),
                        Fact::Setting(main, Some(low), true),
                    ]
                } else {
                    assert_eq!(wanted, saved);
                    [
                        Fact::Unavailable,
                        Fact::Ready(2),
                        Fact::Catalog([false, false]),
                        Fact::Setting(main, Some(saved), true),
                    ]
                };
                let reply = settings_reply(main, current, wanted, sent, eligible, frames)?;
                assert_eq!(reply, Some(wanted), "the write was acknowledged");
                selected.set(wanted);
                Ok(reply)
            },
            |_, _, _, _| panic!("disqualification must prevent every submit"),
            || {
                assert_eq!(selected.get(), saved);
                assert_eq!(writes.get(), 2, "one selection and one restore");
                Ok(())
            },
        );
        assert_eq!(
            result.err().expect("work disqualified").to_string(),
            "target is no longer eligible"
        );
        assert_eq!(
            (outcomes.restore, outcomes.exit),
            ("confirmed", "confirmed")
        );
    }
}

#[test]
fn a_refused_second_selection_restores_after_a_confirmed_current_view() {
    let main = ThreadId(7);
    let sent = Instant::now();
    let saved = (1, Effort::High);
    let temporary = (0, Effort::Low);
    let selected = Cell::new(saved);
    let writes = Cell::new(0);
    let submits = Cell::new(0);
    let mut outcomes = Outcomes::default();
    let result = drive(
        Ok((main, Some(saved), [true, true])),
        20,
        sent,
        &mut outcomes,
        |thread, current, wanted, _, eligible| {
            let n = writes.get();
            writes.set(n + 1);
            assert_eq!(thread, main);
            assert_eq!(current, selected.get());
            let response = match n {
                0 => {
                    assert_eq!((current, wanted), (saved, temporary));
                    vec![Fact::Setting(main, Some(wanted), true)]
                }
                1 => {
                    assert_eq!((current, wanted), (temporary, (1, Effort::Low)));
                    vec![
                        Fact::Refused,
                        Fact::Catalog([true, true]),
                        Fact::Setting(main, Some(temporary), true),
                    ]
                }
                2 => {
                    assert_eq!((current, wanted), (temporary, saved));
                    vec![Fact::Setting(main, Some(saved), true)]
                }
                _ => panic!("only one restore is permitted"),
            };
            let reply = settings_reply(main, current, wanted, sent, eligible, response)?;
            if reply == Some(wanted) {
                selected.set(wanted);
            }
            Ok(reply)
        },
        |thread, turn, target, _| {
            assert_eq!((thread, target, selected.get()), (main, 0, temporary));
            assert_eq!(turn, submits.get() + 1);
            submits.set(turn);
            Ok(Some(Sample {
                run: RunId(u64::from(turn)),
                target,
                pipe_us: 1,
            }))
        },
        || {
            assert_eq!(submits.get(), 10, "no submit for the second target");
            assert_eq!(writes.get(), 3, "orderly refusal still permits one restore");
            assert_eq!(selected.get(), saved, "restore precedes quit");
            Ok(())
        },
    );
    assert_eq!(
        result.err().expect("work refused").to_string(),
        "setting was not accepted"
    );
    assert_eq!(
        (outcomes.restore, outcomes.exit),
        ("confirmed", "confirmed")
    );
}
