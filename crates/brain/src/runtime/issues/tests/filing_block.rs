use super::*;
use crate::coordinator::Effect;
use crate::issues::{Answer as IssueAnswer, Live};
use crate::test_support::Captured;
use kumochi_protocol::message::{IssueRefused, IssueSession};
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStringExt as _;

fn denied(request: u64, action: &Action) -> [ControlToShell; 2] {
    [
        ControlToShell::ActionState {
            request: Some(request),
            draft: action.draft,
            version: action.version,
            state: action.state(),
            approval: action.approval,
        },
        ControlToShell::IssueRefused {
            request,
            draft: action.draft,
            reason: IssueRefused::NotAllowedNow,
            version: action.version,
            approval: action.approval,
        },
    ]
}

fn unknown_draft(scratch: &Scratch, owner: &Action) -> Action {
    let mut store = Store::open(&scratch.db()).unwrap();
    let unknown = store
        .create_draft(
            owner.thread,
            &IssueTitle::new("An older attempt").unwrap(),
            &IssueBody::new("Different content").unwrap(),
            owner.repo.as_ref(),
            owner.account.as_ref(),
        )
        .unwrap();
    let repos = [owner.repo.clone().unwrap()];
    let live = Live {
        capacity: true,
        account: owner.account.as_ref(),
        repos: &repos,
    };
    let IssueAnswer::Changed(preflight) = store
        .consent(
            unknown.draft,
            &unknown.hash(),
            unknown.approval.unwrap(),
            Decision::File,
            &live,
        )
        .unwrap()
    else {
        panic!("fixture enters Preflight");
    };
    let IssueAnswer::Changed(posting) = store
        .preflighted(
            preflight.draft,
            preflight.version,
            Ok(Mark {
                at_s: NonZeroU64::new(120).unwrap(),
                presend: Bounded::default(),
            }),
        )
        .unwrap()
    else {
        panic!("fixture enters Posting");
    };
    let IssueAnswer::Changed(unknown) = store.interrupted(posting.draft, posting.version).unwrap()
    else {
        panic!("fixture enters Unknown");
    };
    assert!(matches!(
        unknown.stage,
        Stage::Unknown {
            lookup: LookupResult::Pending,
            ..
        }
    ));
    unknown
}

#[tokio::test]
async fn live_filing_block_denies_real_confirmation_and_lookup_before_a_job() {
    let scratch = Scratch::new("live-filing-block");
    let server = Arc::new(FilingServer::default());
    let (mut brain, original) = reviewed_filing_owner(&scratch, &server).await;
    brain.filing.set_live_block_for_test(Some(OsStr::new("0")));
    let session = brain.issue_session().unwrap();
    let first = IssueOp::Consent {
        draft: original.draft,
        hash: original.hash(),
        approval: original.approval.unwrap(),
        decision: Decision::File,
    };
    let effects = brain
        .issue_op(session, 1, &first, Some(brain.epoch))
        .unwrap();
    assert_eq!(
        brain.filing.deadline_for_test(),
        None,
        "blocked File must not reserve a job"
    );
    brain.apply(effects, None).unwrap();
    let changed = Action {
        version: StateVersion(3),
        approval: Some(ApprovalRev(3)),
        consumed: original.approval,
        ..original.clone()
    };
    assert_eq!(storage::settle(&mut brain).await, denied(1, &changed));
    let read = |row| {
        Store::open(&scratch.db())
            .unwrap()
            .action(row)
            .unwrap()
            .unwrap()
    };
    assert_eq!(read(changed.draft), changed);
    let missing = |request| {
        [Effect::Control(ControlToShell::IssueUnavailable {
            kind: IssueUnavailableKind::Action,
            request,
            draft: original.draft,
        })]
    };
    assert_eq!(
        brain
            .issue_op(session, 2, &first, Some(brain.epoch))
            .unwrap(),
        missing(2)
    );
    let minted = IssueOp::Consent {
        draft: changed.draft,
        hash: changed.hash(),
        approval: changed.approval.unwrap(),
        decision: Decision::File,
    };
    assert_eq!(
        brain
            .issue_op(session, 3, &minted, Some(brain.epoch))
            .unwrap(),
        missing(3)
    );
    assert_eq!(read(changed.draft), changed);
    assert_eq!(brain.filing.review_counts(), (0, 0));
    let fresh = rearm_filing(&scratch, &mut brain, &changed, 4).await;
    assert_eq!(
        (fresh.version, fresh.approval),
        (StateVersion(4), Some(ApprovalRev(4)))
    );
    assert_eq!(brain.filing.review_counts(), (1, 0));

    let unknown = unknown_draft(&scratch, &fresh);
    let effects = brain
        .issue_op(
            session,
            5,
            &IssueOp::CheckAgain {
                draft: unknown.draft,
            },
            Some(brain.epoch),
        )
        .unwrap();
    assert_eq!(
        brain.filing.deadline_for_test(),
        None,
        "blocked lookup must not reserve a job"
    );
    brain.apply(effects, None).unwrap();
    assert_eq!(storage::settle(&mut brain).await, denied(5, &unknown));
    assert_eq!(read(unknown.draft), unknown);
    assert_eq!(read(fresh.draft), fresh);
    assert_eq!(server.factories.load(Ordering::SeqCst), 0);
    assert_eq!(server.gets.load(Ordering::SeqCst), 0);
    assert_eq!(server.posts.load(Ordering::SeqCst), 0);
    brain.close().await;
}

async fn serve_once(brain: &mut Brain, inputs: &[ControlToBrain]) {
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        open_filing(&mut shell).await;
        for input in inputs {
            shell.send(input).await;
        }
        shell.control_out.shutdown().await.unwrap();
        assert_eq!(shell.control().await, None);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
}

#[tokio::test]
async fn live_filing_block_attests_the_serving_identity_and_latches_even_a_refused_attempt() {
    let scratch = Scratch::new("live-filing-attestation");
    let server = Arc::new(FilingServer::default());
    let (mut brain, original) = filing_owner(&scratch, &server).await;
    let captured = Captured::default();
    let _capture = captured.install();
    let incarnation = brain.issue_session().unwrap().incarnation;
    let non_unicode = OsString::from_wide(&[0xd800]);
    assert!(non_unicode.to_str().is_none());

    brain.filing.set_live_block_for_test(None);
    serve_once(&mut brain, &[]).await;
    assert!(!captured.text().contains("live filing block armed"));
    assert_eq!(brain.issue_session(), None);

    let empty = brain.prepare_issue_session(incarnation);
    brain.filing.set_live_block_for_test(Some(OsStr::new("")));
    serve_once(&mut brain, &[]).await;
    let log = captured.text();
    assert_eq!(log.matches("live filing block armed").count(), 1);
    let armed = log
        .lines()
        .find(|line| line.contains("live filing block armed"))
        .unwrap();
    assert!(armed.contains(&format!("issue_epoch={}", empty.epoch)));
    assert!(armed.contains("attempted=false"));

    let identity = brain.prepare_issue_session(incarnation);
    brain
        .filing
        .set_live_block_for_test(Some(non_unicode.as_os_str()));
    let forged = IssueSession {
        incarnation: Incarnation(9999),
        ..identity
    };
    let attempts = [1, 2].map(|request| ControlToBrain::Issue {
        session: forged,
        request,
        op: Box::new(IssueOp::CheckAgain {
            draft: original.draft,
        }),
    });
    serve_once(&mut brain, &attempts).await;
    let log = captured.text();
    let live: Vec<_> = log
        .lines()
        .filter(|line| line.contains("live filing"))
        .collect();
    assert_eq!(live.len(), 3);
    assert!(live[1].contains("live filing block armed"));
    assert!(live[1].contains("attempted=false"));
    assert!(live[2].contains("live filing attempt blocked"));
    for line in &live[1..] {
        assert!(line.contains(kumochi_protocol::BUILD_ID));
        assert!(line.contains(&format!("pid={}", std::process::id())));
        assert!(line.contains(&format!("incarnation={}", identity.incarnation.0)));
        assert!(line.contains(&format!("issue_epoch={}", identity.epoch)));
        assert!(!line.contains("incarnation=9999"));
        assert!(!line.contains(original.title.as_str()));
        assert!(!line.contains("owner/repo"));
    }
    let replacement = brain.prepare_issue_session(identity.incarnation);
    assert_ne!(replacement.epoch, identity.epoch);
    serve_once(&mut brain, &[]).await;
    let log = captured.text();
    let armed = log
        .lines()
        .rev()
        .find(|line| line.contains("live filing block armed"))
        .unwrap();
    assert!(armed.contains(&format!("issue_epoch={}", replacement.epoch)));
    assert!(armed.contains("attempted=true"));
    assert_eq!(log.matches("live filing attempt blocked").count(), 1);
    assert_eq!(brain.filing.deadline_for_test(), None);
    brain.close().await;
}
