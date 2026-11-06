//! Admission and outcomes: the content hash, consent with its ordered checks and used-up
//! revisions, bind across live threads, and the send completions and lookups, each fenced to its
//! dispatch.

use kumochi_protocol::message::{
    ActionState, ApprovalRev, Decision, DraftId, FailReason, GitHubLogin, IssueRefused, IssueUrl,
    LookupResult, RepoName, StateVersion, ThreadId,
};
use sha2::{Digest as _, Sha256};

use super::actions_tests::{
    A, B, C, body, draft, issue, mark, opened, posting, repo, seed, snapshot, thread, title,
    uncertain, unknown, user,
};
use super::{Store, StoreError};
use crate::connections::github::api::GitHubUser;
use crate::connections::github::issues::CreateOutcome;
use crate::connections::http_client::ResponseMeta;
use crate::issues::{Action, Answer, Live, Lookup, Stage};
use crate::test_support::Scratch;

fn created(number: u64) -> CreateOutcome {
    CreateOutcome::Created {
        number,
        url: IssueUrl::for_issue(&repo(), number).expect("url"),
    }
}

fn definite(reason: FailReason) -> CreateOutcome {
    CreateOutcome::Definite {
        reason,
        status: Some(403),
        meta: ResponseMeta::default(),
    }
}

fn url(number: u64) -> IssueUrl {
    IssueUrl::new(&format!("https://github.com/octo/repo/issues/{number}")).expect("url")
}

fn changed(answer: Result<Answer, StoreError>) -> Action {
    match answer {
        Ok(Answer::Changed(action)) => action,
        other => panic!("expected a change: {other:?}"),
    }
}

/// Runs `op`, which must be refused with `why` and leave every row and the mark as they were.
fn refused_unwritten(
    store: &mut Store,
    why: IssueRefused,
    op: impl FnOnce(&mut Store) -> Result<Answer, StoreError>,
) {
    let before = snapshot(&store.conn);
    let answer = op(store);
    assert!(
        matches!(answer, Ok(Answer::Refused(got, _)) if got == why),
        "{why:?}: {answer:?}"
    );
    assert_eq!(snapshot(&store.conn), before, "{why:?} wrote");
}

/// Decides with the action's current revision and hash, as user 7 with octo/repo listed.
fn decide(store: &mut Store, action: &Action, decision: Decision) -> Result<Answer, StoreError> {
    let (account, repos) = (user(7), [repo()]);
    let live = Live {
        account: Some(&account),
        repos: &repos,
    };
    let approval = action.approval.expect("offered");
    store.consent(action.draft, &action.hash(), approval, decision, &live)
}

/// A draft whose first send ended uncertain: Unknown with mark A and one uncertain attempt.
fn unknown_once(store: &mut Store, thread: ThreadId) -> Action {
    let first = draft(store, thread);
    let preflight = changed(decide(store, &first, Decision::File));
    let posting = changed(store.preflighted(first.draft, preflight.version, Ok(mark(A))));
    changed(store.posted(first.draft, posting.version, &uncertain()))
}

#[test]
fn the_hash_binds_exactly_repository_account_title_and_body() {
    let scratch = Scratch::new("admission-hash");
    let mut store = opened(&scratch);
    let thread = thread(&mut store);
    let widest = GitHubUser {
        id: u64::MAX,
        login: GitHubLogin::new("renamed").expect("login"),
    };
    let unbound = store.create_draft(thread, &title("T"), &body("B"), None, None);
    let wide = store.create_draft(
        thread,
        &title("T"),
        &body("B\nC"),
        Some(&repo()),
        Some(&widest),
    );
    let hashes = [
        draft(&mut store, thread),
        unbound.expect("draft"),
        wide.expect("draft"),
    ]
    .map(|action| action.hash().0);
    let sha = |json: &str| -> [u8; 32] { Sha256::digest(json).into() };
    assert_eq!(
        hashes,
        [
            sha(r#"{"repo":"octo/repo","account":7,"title":"T","body":"B"}"#),
            sha(r#"{"repo":null,"account":null,"title":"T","body":"B"}"#),
            sha(r#"{"repo":"octo/repo","account":18446744073709551615,"title":"T","body":"B\nC"}"#),
        ]
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "one scenario, read top to bottom")]
fn consent_checks_in_order_and_uses_up_the_current_revision() {
    let scratch = Scratch::new("admission-consent");
    let mut store = opened(&scratch);
    let thread = thread(&mut store);
    let owner = user(u64::MAX);
    let first = store
        .create_draft(thread, &title("T"), &body("B"), Some(&repo()), Some(&owner))
        .expect("draft");
    let (zero, namesake) = (user(0), user(u64::MAX - 1));
    let (listed, other) = ([repo()], [RepoName::new("octo/other").expect("repo")]);
    let altered = |title_text: &str, body_text: &str| {
        Action {
            title: title(title_text),
            body: body(body_text),
            ..first.clone()
        }
        .hash()
    };
    let refusals = [
        (
            altered("T.", "B"),
            Some(&owner),
            &listed,
            IssueRefused::ContentChanged,
        ),
        (
            altered("T", "B."),
            Some(&owner),
            &listed,
            IssueRefused::ContentChanged,
        ),
        (first.hash(), None, &listed, IssueRefused::NotConnected),
        (
            first.hash(),
            Some(&zero),
            &listed,
            IssueRefused::NotConnected,
        ),
        (
            first.hash(),
            Some(&namesake),
            &listed,
            IssueRefused::OtherAccount,
        ),
        (
            first.hash(),
            Some(&owner),
            &other,
            IssueRefused::RepoNotListed,
        ),
    ];
    let mut current = first.clone();
    for (n, (hash, account, repos, why)) in (1_u64..).zip(refusals) {
        let live = Live { account, repos };
        let answer = store.consent(first.draft, &hash, ApprovalRev(n), Decision::File, &live);
        current = Action {
            version: StateVersion(n + 1),
            approval: Some(ApprovalRev(n + 1)),
            consumed: Some(ApprovalRev(n)),
            ..current
        };
        assert!(
            matches!(&answer, Ok(Answer::Refused(got, action)) if *got == why && *action == current),
            "{why:?}: {answer:?}"
        );
    }
    let ok = Live {
        account: Some(&owner),
        repos: &listed,
    };
    let consent = |store: &mut Store, rev: u64| {
        store.consent(
            first.draft,
            &first.hash(),
            ApprovalRev(rev),
            Decision::File,
            &ok,
        )
    };
    let before = snapshot(&store.conn);
    let replay = consent(&mut store, 6);
    assert!(
        matches!(&replay, Ok(Answer::Unchanged(action)) if *action == current),
        "the last revision used up can't file once its cause is fixed: {replay:?}"
    );
    assert_eq!(snapshot(&store.conn), before);
    for stale in [0, 5, 8] {
        refused_unwritten(&mut store, IssueRefused::StaleApproval, |store| {
            consent(store, stale)
        });
    }
    let sending = changed(consent(&mut store, 7));
    assert_eq!(
        sending,
        Action {
            version: StateVersion(8),
            stage: Stage::Preflight { unresolved: None },
            approval: None,
            consumed: Some(ApprovalRev(7)),
            ..current
        }
    );
    let before = snapshot(&store.conn);
    let again = consent(&mut store, 7);
    assert!(
        matches!(&again, Ok(Answer::Unchanged(action)) if *action == sending),
        "{again:?}"
    );
    assert_eq!(snapshot(&store.conn), before);
}

#[test]
fn completions_land_only_at_their_dispatch_version() {
    let scratch = Scratch::new("admission-fence");
    let mut store = opened(&scratch);
    let thread = thread(&mut store);
    let first = draft(&mut store, thread);
    let id = first.draft;
    let entered = changed(decide(&mut store, &first, Decision::File));
    assert_eq!(entered.version, StateVersion(2));
    let failed = changed(store.preflighted(id, StateVersion(2), Err(FailReason::Network)));
    assert_eq!(failed.state(), ActionState::Failed(FailReason::Network));
    let again = changed(decide(&mut store, &failed, Decision::File));
    assert_eq!(again.version, StateVersion(4));
    for at in [2, 5, u64::MAX].map(StateVersion) {
        refused_unwritten(&mut store, IssueRefused::NotAllowedNow, |store| {
            store.preflighted(id, at, Ok(mark(A)))
        });
        refused_unwritten(&mut store, IssueRefused::NotAllowedNow, |store| {
            store.interrupted(id, at)
        });
    }
    let posting = changed(store.preflighted(id, StateVersion(4), Ok(mark(B))));
    assert_eq!(
        (posting.stage, posting.version),
        (
            Stage::Posting {
                mark: mark(B),
                unresolved: None
            },
            StateVersion(5)
        )
    );
    for at in [2, 4, 6, u64::MAX].map(StateVersion) {
        refused_unwritten(&mut store, IssueRefused::NotAllowedNow, |store| {
            store.posted(id, at, &created(7))
        });
        refused_unwritten(&mut store, IssueRefused::NotAllowedNow, |store| {
            store.interrupted(id, at)
        });
    }
    let landed = changed(store.posted(id, StateVersion(5), &created(7)));
    assert_eq!(
        landed.state(),
        ActionState::Filed {
            number: 7,
            url: url(7),
            confirmed_by_user: false
        }
    );
    refused_unwritten(&mut store, IssueRefused::NotAllowedNow, |store| {
        store.interrupted(id, StateVersion(5))
    });
    let second = draft(&mut store, thread);
    let entered = changed(decide(&mut store, &second, Decision::File));
    let posting = changed(store.preflighted(second.draft, entered.version, Ok(mark(A))));
    let unknown = changed(store.interrupted(second.draft, posting.version));
    assert_eq!(
        (unknown.stage, unknown.uncertain),
        (
            Stage::Unknown {
                mark: mark(A),
                lookup: LookupResult::Pending
            },
            1
        )
    );
    refused_unwritten(&mut store, IssueRefused::NotAllowedNow, |store| {
        store.posted(second.draft, posting.version, &created(7))
    });
}

/// Each stage with what each completion turns it into; `None` is a refusal that writes nothing.
#[test]
#[expect(clippy::too_many_lines, reason = "one table, read top to bottom")]
fn each_completion_takes_only_its_stage() {
    let scratch = Scratch::new("admission-stages");
    let mut store = opened(&scratch);
    let thread = thread(&mut store);
    let unknown_a = || {
        Some((
            Stage::Unknown {
                mark: mark(A),
                lookup: LookupResult::Pending,
            },
            1,
        ))
    };
    let cases = [
        (r#"{"kind":"draft"}"#.to_owned(), 0, [const { None }; 5]),
        (
            r#"{"kind":"preflight","unresolved":null}"#.to_owned(),
            0,
            [
                Some((
                    Stage::Posting {
                        mark: mark(B),
                        unresolved: None,
                    },
                    0,
                )),
                None,
                None,
                Some((
                    Stage::Failed {
                        reason: FailReason::Interrupted,
                    },
                    0,
                )),
                None,
            ],
        ),
        (
            posting(A),
            0,
            [
                None,
                Some((
                    Stage::Failed {
                        reason: FailReason::PermissionDenied,
                    },
                    0,
                )),
                unknown_a(),
                unknown_a(),
                None,
            ],
        ),
        (
            r#"{"kind":"filed","number":7,"confirmed":false}"#.to_owned(),
            0,
            [const { None }; 5],
        ),
        (
            r#"{"kind":"failed","reason":"Network"}"#.to_owned(),
            0,
            [const { None }; 5],
        ),
        (
            unknown(r#""Pending""#),
            1,
            [
                None,
                None,
                None,
                None,
                Some((
                    Stage::Filed {
                        number: issue(7),
                        confirmed: false,
                    },
                    1,
                )),
            ],
        ),
        (r#"{"kind":"discarded"}"#.to_owned(), 1, [const { None }; 5]),
    ];
    for (stage, count, expected) in cases {
        for (input, expected) in expected.into_iter().enumerate() {
            let id = draft(&mut store, thread).draft;
            seed(&store, id, &stage, count);
            let before = snapshot(&store.conn);
            let at = StateVersion(1);
            let answer = match input {
                0 => store.preflighted(id, at, Ok(mark(B))),
                1 => store.posted(id, at, &definite(FailReason::PermissionDenied)),
                2 => store.posted(id, at, &uncertain()),
                3 => store.interrupted(id, at),
                _ => store.looked_up(id, count, Lookup::Placed(issue(7))),
            };
            match (answer, expected) {
                (Ok(Answer::Changed(action)), Some(to)) => {
                    assert_eq!((action.stage, action.uncertain), to, "{stage} {input}");
                }
                (Ok(Answer::Refused(IssueRefused::NotAllowedNow, _)), None) => {
                    assert_eq!(snapshot(&store.conn), before, "{stage} {input}");
                }
                (other, _) => panic!("{stage} {input}: {other:?}"),
            }
        }
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "one scenario, read top to bottom")]
fn file_anyway_keeps_the_earliest_unresolved_mark() {
    type End = fn(&mut Store, DraftId, StateVersion) -> Result<Answer, StoreError>;
    let scratch = Scratch::new("admission-anyway");
    let mut store = opened(&scratch);
    let thread = thread(&mut store);
    let mut current = unknown_once(&mut store, thread);
    let id = current.draft;
    refused_unwritten(&mut store, IssueRefused::NotAllowedNow, |store| {
        store.edit_draft(id, &title("E"), &body("E"), None)
    });
    let before = snapshot(&store.conn);
    let bound = store.bind_drafts(Some(&user(8)), Some(&repo()));
    assert_eq!(bound.expect("bind"), []);
    assert_eq!(
        snapshot(&store.conn),
        before,
        "bind skips an unresolved action"
    );
    let Ok(Answer::Refused(IssueRefused::NotAllowedNow, replaced)) =
        decide(&mut store, &current, Decision::File)
    else {
        panic!("ordinary File");
    };
    current = Action {
        version: StateVersion(5),
        approval: Some(ApprovalRev(3)),
        consumed: current.approval,
        ..current
    };
    assert_eq!(replaced, current, "only the revision and version moved");
    let unknown_a = Stage::Unknown {
        mark: mark(A),
        lookup: LookupResult::Pending,
    };
    let ends: [(Option<&str>, End, Stage, u32); 7] = [
        (
            None,
            |store, id, at| store.preflighted(id, at, Err(FailReason::PreflightFailed)),
            unknown_a.clone(),
            1,
        ),
        (
            Some(B),
            |store, id, at| store.posted(id, at, &definite(FailReason::Rejected)),
            unknown_a.clone(),
            1,
        ),
        (
            Some(C),
            |store, id, at| store.posted(id, at, &uncertain()),
            unknown_a.clone(),
            2,
        ),
        (
            None,
            |store, id, at| store.interrupted(id, at),
            unknown_a.clone(),
            2,
        ),
        (
            Some(B),
            |store, id, at| store.interrupted(id, at),
            unknown_a.clone(),
            3,
        ),
        (
            Some(B),
            |store, id, at| {
                let zero = CreateOutcome::Created {
                    number: 0,
                    url: url(1),
                };
                store.posted(id, at, &zero)
            },
            unknown_a,
            4,
        ),
        (
            Some(C),
            |store, id, at| store.posted(id, at, &created(9)),
            Stage::Filed {
                number: issue(9),
                confirmed: false,
            },
            4,
        ),
    ];
    for (sent, end, stage, count) in ends {
        let preflight = changed(decide(&mut store, &current, Decision::FileAnyway));
        assert_eq!(
            preflight.stage,
            Stage::Preflight {
                unresolved: Some(mark(A))
            }
        );
        let sending = match sent {
            Some(json) => {
                let posting = changed(store.preflighted(id, preflight.version, Ok(mark(json))));
                let expected = Stage::Posting {
                    mark: mark(json),
                    unresolved: Some(mark(A)),
                };
                assert_eq!(posting.stage, expected);
                posting
            }
            None => preflight,
        };
        current = changed(end(&mut store, id, sending.version));
        assert_eq!((&current.stage, current.uncertain), (&stage, count));
    }
}

#[test]
fn lookups_land_only_in_their_episode() {
    let scratch = Scratch::new("admission-lookup");
    let mut store = opened(&scratch);
    let thread = thread(&mut store);
    let id = unknown_once(&mut store, thread).draft;
    for episode in [0, 2] {
        refused_unwritten(&mut store, IssueRefused::NotAllowedNow, |store| {
            store.looked_up(id, episode, Lookup::Placed(issue(7)))
        });
    }
    let none = changed(store.looked_up(id, 1, Lookup::None));
    assert_eq!(
        none.stage,
        Stage::Unknown {
            mark: mark(A),
            lookup: LookupResult::None
        }
    );
    let Ok(Answer::Refused(IssueRefused::NotAllowedNow, refused)) =
        decide(&mut store, &none, Decision::ThisIsTheIssue)
    else {
        panic!("no candidate to confirm");
    };
    let account = user(7);
    let unlisted = Live {
        account: Some(&account),
        repos: &[],
    };
    let approval = refused.approval.expect("offered");
    let anyway = store.consent(
        id,
        &refused.hash(),
        approval,
        Decision::FileAnyway,
        &unlisted,
    );
    let Ok(Answer::Refused(IssueRefused::RepoNotListed, refused)) = anyway else {
        panic!("{anyway:?}");
    };
    assert_eq!((refused.version, refused.uncertain), (StateVersion(7), 1));
    let first = changed(store.looked_up(id, 1, Lookup::Unplaced(issue(7))));
    let second = changed(store.looked_up(id, 1, Lookup::Unplaced(issue(8))));
    assert_eq!(
        second.state(),
        ActionState::Unknown {
            lookup: LookupResult::Unplaced {
                number: 8,
                url: url(8)
            },
            maybe_duplicate: false
        }
    );
    refused_unwritten(&mut store, IssueRefused::StaleApproval, |store| {
        decide(store, &first, Decision::ThisIsTheIssue)
    });
    let confirmed = changed(decide(&mut store, &second, Decision::ThisIsTheIssue));
    assert_eq!(
        confirmed.state(),
        ActionState::Filed {
            number: 8,
            url: url(8),
            confirmed_by_user: true
        }
    );
    refused_unwritten(&mut store, IssueRefused::NotAllowedNow, |store| {
        store.looked_up(id, 1, Lookup::Placed(issue(9)))
    });
    let placed = unknown_once(&mut store, thread).draft;
    let filed = changed(store.looked_up(placed, 1, Lookup::Placed(issue(9))));
    let expected = Stage::Filed {
        number: issue(9),
        confirmed: false,
    };
    assert_eq!((filed.stage, filed.uncertain), (expected, 1));
    let dropped = unknown_once(&mut store, thread).draft;
    let several = changed(store.looked_up(dropped, 1, Lookup::Several));
    assert_eq!(
        several.stage,
        Stage::Unknown {
            mark: mark(A),
            lookup: LookupResult::Several
        }
    );
    assert!(matches!(
        store.discard_draft(dropped),
        Ok(Answer::Changed(_))
    ));
    refused_unwritten(&mut store, IssueRefused::NotAllowedNow, |store| {
        store.looked_up(dropped, 1, Lookup::Failed)
    });
}

#[test]
fn bind_reaches_every_live_draft_and_files_nothing() {
    let scratch = Scratch::new("admission-bind");
    let mut store = opened(&scratch);
    let (one, two) = (thread(&mut store), thread(&mut store));
    let chosen_repo = RepoName::new("octo/chosen").expect("repo");
    let bare = store
        .create_draft(one, &title("T"), &body("B"), None, None)
        .expect("draft");
    let chosen = store
        .create_draft(two, &title("T"), &body("B"), Some(&chosen_repo), None)
        .expect("draft");
    let sent = draft(&mut store, two);
    let entered = changed(decide(&mut store, &sent, Decision::File));
    let failed = changed(store.preflighted(sent.draft, entered.version, Err(FailReason::Network)));
    let unresolved = unknown_once(&mut store, one);
    let (seven, eight) = (user(7), user(8));
    let listed = [repo(), chosen_repo.clone()];
    let live = Live {
        account: Some(&seven),
        repos: &listed,
    };
    let consent = |store: &mut Store, action: &Action| {
        let approval = action.approval.expect("offered");
        store.consent(
            action.draft,
            &action.hash(),
            approval,
            Decision::File,
            &live,
        )
    };
    let Ok(Answer::Refused(IssueRefused::NotConnected, bare)) = consent(&mut store, &bare) else {
        panic!("no account is bound");
    };
    let bound = store.bind_drafts(Some(&seven), None).expect("bind");
    let (bare, chosen) = (
        Action {
            version: StateVersion(3),
            approval: Some(ApprovalRev(8)),
            account: Some(seven.clone()),
            ..bare
        },
        Action {
            version: StateVersion(2),
            approval: Some(ApprovalRev(9)),
            account: Some(seven.clone()),
            ..chosen
        },
    );
    assert_eq!(bound, [bare.clone(), chosen.clone()]);
    for kept in [&failed, &unresolved] {
        assert_eq!(store.action(kept.draft).expect("read").as_ref(), Some(kept));
    }
    let Ok(Answer::Refused(IssueRefused::RepoNotListed, bare)) = consent(&mut store, &bare) else {
        panic!("no repository is bound");
    };
    let bound = store.bind_drafts(None, Some(&repo())).expect("bind");
    let bare = Action {
        version: StateVersion(5),
        approval: Some(ApprovalRev(11)),
        repo: Some(repo()),
        ..bare
    };
    assert_eq!(bound, std::slice::from_ref(&bare));
    let sending = changed(consent(&mut store, &bare));
    assert_eq!(sending.stage, Stage::Preflight { unresolved: None });
    let before = snapshot(&store.conn);
    assert_eq!(store.bind_drafts(None, Some(&repo())).expect("bind"), []);
    assert_eq!(
        snapshot(&store.conn),
        before,
        "a bind that changes nothing writes nothing"
    );
    let rebound = store
        .bind_drafts(Some(&eight), Some(&repo()))
        .expect("bind");
    let switched = Action {
        version: StateVersion(3),
        approval: Some(ApprovalRev(12)),
        account: Some(eight.clone()),
        ..chosen.clone()
    };
    let retried = Action {
        version: StateVersion(4),
        stage: Stage::Draft,
        approval: Some(ApprovalRev(13)),
        account: Some(eight.clone()),
        ..failed
    };
    assert_eq!(rebound, [switched.clone(), retried]);
    for kept in [&sending, &unresolved] {
        assert_eq!(store.action(kept.draft).expect("read").as_ref(), Some(kept));
    }
    refused_unwritten(&mut store, IssueRefused::StaleApproval, |store| {
        consent(store, &chosen)
    });
    let other = consent(&mut store, &switched);
    assert!(
        matches!(other, Ok(Answer::Refused(IssueRefused::OtherAccount, _))),
        "{other:?}"
    );
}

#[test]
fn a_revision_used_before_a_restart_stays_used() {
    let scratch = Scratch::new("admission-restart");
    let mut store = opened(&scratch);
    let thread = thread(&mut store);
    let first = draft(&mut store, thread);
    let entered = changed(decide(&mut store, &first, Decision::File));
    changed(store.preflighted(first.draft, entered.version, Ok(mark(A))));
    drop(store);
    let mut store = opened(&scratch);
    let unknown = store.action(first.draft).expect("read").expect("live");
    assert_eq!(
        (
            &unknown.stage,
            unknown.uncertain,
            unknown.consumed,
            unknown.approval
        ),
        (
            &Stage::Unknown {
                mark: mark(A),
                lookup: LookupResult::Pending
            },
            1,
            Some(ApprovalRev(1)),
            Some(ApprovalRev(2))
        )
    );
    let before = snapshot(&store.conn);
    for decision in [Decision::File, Decision::FileAnyway] {
        let replay = decide(&mut store, &first, decision);
        assert!(
            matches!(&replay, Ok(Answer::Unchanged(action)) if *action == unknown),
            "{decision:?}: {replay:?}"
        );
    }
    assert_eq!(snapshot(&store.conn), before);
    let anyway = changed(decide(&mut store, &unknown, Decision::FileAnyway));
    assert_eq!(
        anyway.stage,
        Stage::Preflight {
            unresolved: Some(mark(A))
        }
    );
}
