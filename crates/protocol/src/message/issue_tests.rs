use kumochi_scene::text::Bounded;
use serde::{Serialize, de::DeserializeOwned};

use super::*;
use crate::frame::{HEADER_LEN, Pipe, encode};
use crate::tap::{Direction, Record, Tapped, write};

#[test]
fn an_unresolved_draft_refusal_appends_tag_seven_and_round_trips_its_identity() {
    use crate::frame::{FrameError, read};
    assert_eq!(
        postcard::to_allocvec(&IssueRefused::ThreadGone).unwrap(),
        [6]
    );
    let reason = IssueRefused::UnresolvedDraft {
        draft: DraftId(128),
    };
    assert_eq!(postcard::to_allocvec(&reason).unwrap(), [7, 128, 1]);
    let json = serde_json::json!({"UnresolvedDraft":{"draft":128}});
    assert_eq!(serde_json::to_value(reason).unwrap(), json);
    assert_eq!(
        serde_json::from_value::<IssueRefused>(json).unwrap(),
        reason
    );
    let message = ControlToShell::IssueRefused {
        request: 128,
        draft: DraftId(2),
        version: StateVersion(3),
        approval: Some(ApprovalRev(4)),
        reason,
    };
    let wire = encode(&message, Pipe::Control).unwrap();
    assert_eq!(wire, [10, 0, 0, 0, 19, 128, 1, 2, 3, 1, 4, 7, 128, 1]);
    assert_eq!(
        read::<ControlToShell>(&mut wire.as_slice(), Pipe::Control).unwrap(),
        Some(message)
    );
    for length in 1..wire.len() {
        assert!(matches!(
            read::<ControlToShell>(&mut &wire[..length], Pipe::Control),
            Err(FrameError::Truncated)
        ));
    }
    let largest = IssueRefused::UnresolvedDraft {
        draft: DraftId(u64::MAX),
    };
    let mut expected = vec![7];
    expected.extend([255; 9]);
    expected.push(1);
    assert_eq!(postcard::to_allocvec(&largest).unwrap(), expected);
    assert_eq!(
        postcard::from_bytes::<IssueRefused>(&expected).unwrap(),
        largest
    );
    assert!(postcard::from_bytes::<IssueRefused>(&[8]).is_err());
    assert!(
        serde_json::from_str::<IssueRefused>(
            r#"{"UnresolvedDraft":{"draft":18446744073709551616}}"#
        )
        .is_err()
    );
}

#[test]
fn an_unchecked_saved_account_appends_tag_six_and_unknown_tag_seven_is_refused() {
    use crate::frame::{FrameError, read};
    let login = GitHubLogin::new("saved").unwrap();
    assert_eq!(
        postcard::to_allocvec(&GitHubView::NeedsSignIn {
            login: login.clone()
        })
        .unwrap(),
        [5, 5, b's', b'a', b'v', b'e', b'd']
    );
    let view = GitHubView::NotVerified { login };
    let message = ControlToShell::GitHub(view.clone());
    let wire = encode(&message, Pipe::Control).unwrap();
    private(&message, Pipe::Control, Direction::ToShell, &["saved"]);
    assert_eq!(wire, [8, 0, 0, 0, 17, 6, 5, b's', b'a', b'v', b'e', b'd']);
    assert_eq!(
        read::<ControlToShell>(&mut wire.as_slice(), Pipe::Control).unwrap(),
        Some(message)
    );
    let json = serde_json::json!({"NotVerified":{"login":"saved"}});
    assert_eq!(serde_json::to_value(&view).unwrap(), json);
    assert_eq!(serde_json::from_value::<GitHubView>(json).unwrap(), view);
    for length in 1..wire.len() {
        assert!(matches!(
            read::<ControlToShell>(&mut &wire[..length], Pipe::Control),
            Err(FrameError::Truncated)
        ));
    }
    let mut unknown = wire;
    unknown[5] = 7;
    assert!(matches!(
        read::<ControlToShell>(&mut unknown.as_slice(), Pipe::Control),
        Err(FrameError::Malformed(_))
    ));
}

#[test]
fn tagged_github_refusal_appends_wire_tags_and_keeps_error_validation() {
    use crate::frame::{FrameError, read};
    let refusal = ControlToShell::GitHubError(ErrorKey::GitHubUnavailable.into());
    let wire = encode(&refusal, Pipe::Control).unwrap();
    assert_eq!(wire, [4, 0, 0, 0, 25, 42, 0, 0]);
    assert_eq!(
        read::<ControlToShell>(&mut wire.as_slice(), Pipe::Control).unwrap(),
        Some(refusal)
    );
    for length in 1..wire.len() {
        assert!(matches!(
            read::<ControlToShell>(&mut &wire[..length], Pipe::Control),
            Err(FrameError::Truncated)
        ));
    }
    assert_eq!(
        read::<ControlToShell>(&mut &[][..], Pipe::Control).unwrap(),
        None
    );
    for (message, tag) in [
        (ControlToShell::Error(ErrorKey::SettingsRefused.into()), 8),
        (
            ControlToShell::GitHub(GitHubView::SignedOut { cleanup: None }),
            17,
        ),
        (
            ControlToShell::AnthropicKeyStageDone {
                plan: KeyStageId([0; 16]),
                result: Err(KeyStageFailure::Unavailable),
            },
            24,
        ),
    ] {
        let encoded = encode(&message, Pipe::Control).unwrap();
        assert_eq!(encoded[HEADER_LEN], tag);
        assert_eq!(
            read::<ControlToShell>(&mut encoded.as_slice(), Pipe::Control).unwrap(),
            Some(message)
        );
    }
    assert_eq!(
        postcard::to_allocvec(&ErrorKey::VoiceUnavailable).unwrap(),
        [41]
    );
    assert_eq!(
        postcard::to_allocvec(&ErrorKey::VoiceModelPath).unwrap(),
        [43]
    );
    assert_eq!(
        postcard::to_allocvec(&ErrorKey::DrawingReplacementFailed).unwrap(),
        [44]
    );
    assert_eq!(
        postcard::to_allocvec(&ErrorKey::DrawingNotReturned).unwrap(),
        [45]
    );
    let mut unknown = wire;
    unknown[5] = 46;
    assert!(matches!(
        read::<ControlToShell>(&mut unknown.as_slice(), Pipe::Control),
        Err(FrameError::Malformed(_))
    ));
    let too_many = [14, 0, 0, 0, 25, 42, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    assert!(matches!(
        read::<ControlToShell>(&mut too_many.as_slice(), Pipe::Control),
        Err(FrameError::Malformed(postcard::Error::SerdeDeCustom))
    ));
}

#[test]
fn filed_confirmation_and_duplicate_warning_round_trip_independently() {
    for confirmed_by_user in [false, true] {
        for maybe_duplicate in [false, true] {
            let value = ActionState::Filed {
                number: 7,
                url: IssueUrl::for_issue(&RepoName::new("o/r").unwrap(), 7).unwrap(),
                confirmed_by_user,
                maybe_duplicate,
            };
            let bytes = postcard::to_allocvec(&value).unwrap();
            assert_eq!(postcard::from_bytes::<ActionState>(&bytes).unwrap(), value);
            assert!(postcard::from_bytes::<ActionState>(&bytes[..bytes.len() - 1]).is_err());
        }
    }
}

#[test]
fn correlated_cards_keep_legacy_indices_and_the_original_input_bound() {
    let empty = DataToBrain::IssueCardsRequest {
        request: 9,
        drafts: Bounded::default(),
    };
    let wire = postcard::to_allocvec(&empty).unwrap();
    assert_eq!(wire, [8, 9, 0]);
    assert_eq!(postcard::from_bytes::<DataToBrain>(&wire).unwrap(), empty);
    let legacy = DataToBrain::IssueCards {
        drafts: Bounded::default(),
    };
    assert_eq!(postcard::to_allocvec(&legacy).unwrap(), [6, 0]);
    for count in [16, 17] {
        let mut raw = vec![8];
        raw.extend(postcard::to_allocvec(&(9_u32, vec![DraftId(1); count])).unwrap());
        if count == 16 {
            assert_eq!(
                postcard::from_bytes::<DataToBrain>(&raw).unwrap(),
                DataToBrain::IssueCardsRequest {
                    request: 9,
                    drafts: Bounded::new(vec![DraftId(1); 16]).unwrap()
                }
            );
        } else {
            assert!(postcard::from_bytes::<DataToBrain>(&raw).is_err());
        }
        for length in 0..raw.len() {
            assert!(postcard::from_bytes::<DataToBrain>(&raw[..length]).is_err());
        }
    }
    for (status, tag) in [
        (IssueCardsStatus::Complete, 0),
        (IssueCardsStatus::Failed, 1),
    ] {
        let done = DataToShell::IssueCardsDone { request: 9, status };
        assert_eq!(postcard::to_allocvec(&done).unwrap(), [19, 9, tag]);
        assert_eq!(
            postcard::from_bytes::<DataToShell>(&[19, 9, tag]).unwrap(),
            done
        );
    }
    assert!(postcard::from_bytes::<DataToShell>(&[19, 9, 2]).is_err());
}

fn checked<T>(construct: fn(&str) -> Result<T, IssueValueError>, good: &[String], bad: &[String])
where
    T: TryFrom<String, Error = IssueValueError>
        + Serialize
        + DeserializeOwned
        + PartialEq
        + std::fmt::Debug,
{
    for text in good {
        let value = construct(text).expect("valid constructor");
        assert_eq!(T::try_from(text.clone()).expect("valid conversion"), value);
        let wire = postcard::to_allocvec(text).expect("raw text");
        assert_eq!(
            postcard::from_bytes::<T>(&wire).expect("valid decode"),
            value
        );
        assert_eq!(postcard::to_allocvec(&value).expect("encode"), wire);
        assert!(postcard::from_bytes::<T>(&wire[..wire.len() - 1]).is_err());
    }
    for text in bad {
        assert_eq!(construct(text), Err(IssueValueError));
        assert_eq!(T::try_from(text.clone()), Err(IssueValueError));
        assert!(
            postcard::from_bytes::<T>(&postcard::to_allocvec(text).expect("raw text")).is_err()
        );
    }
    assert!(postcard::from_bytes::<T>(&[]).is_err());
    assert!(serde_json::from_str::<T>("12").is_err());
}

#[test]
fn repository_login_and_code_validate_on_constructor_and_decode() {
    checked::<RepoName>(
        RepoName::new,
        &[
            "a/b".into(),
            format!("{}/{}", "a".repeat(39), "b".repeat(100)),
            "A/._-x".into(),
        ],
        &[
            String::new(),
            "/b".into(),
            "a/".into(),
            "-a/b".into(),
            "a/.".into(),
            "a/..".into(),
            "a/b/c".into(),
            "é/b".into(),
            "a/b?".into(),
            format!("{}/b", "a".repeat(40)),
            format!("a/{}", "b".repeat(101)),
        ],
    );
    checked::<GitHubLogin>(
        GitHubLogin::new,
        &["A-0".into(), "a".repeat(39)],
        &[
            String::new(),
            "-a".into(),
            "a_b".into(),
            "a/b".into(),
            "é".into(),
            "a".repeat(40),
        ],
    );
    checked::<UserCode>(
        UserCode::new,
        &["A".into(), format!("{}Z", "A9-".repeat(5))],
        &[
            String::new(),
            "A".repeat(17),
            "lower".into(),
            "A B".into(),
            "Á".into(),
        ],
    );
}

#[test]
fn title_counts_scalars_and_preserves_nonblank_text() {
    checked::<IssueTitle>(
        IssueTitle::new,
        &["a".into(), " a ".into(), "🦀".repeat(256)],
        &[
            String::new(),
            "   ".into(),
            " \t ".into(),
            "🦀".repeat(257),
            "a\nb".into(),
            "a\0b".into(),
            "a\u{2028}b".into(),
            "a\u{2029}b".into(),
        ],
    );
    assert_eq!(IssueTitle::new(" a ").expect("valid").as_str(), " a ");
}

#[test]
fn body_normalizes_only_at_construction_and_bounds_utf8_bytes() {
    checked::<IssueBody>(
        |text| IssueBody::try_from(text.to_owned()),
        &[String::new(), "\n\t".into(), "🦀".repeat(8192)],
        &[
            "a".repeat(32769),
            format!("{}a", "🦀".repeat(8192)),
            "x\r".into(),
            "a\0".into(),
            "\r\n".into(),
        ],
    );
    assert_eq!(
        IssueBody::new("a\r\nb\t\n").expect("normalized").as_str(),
        "a\nb\t\n"
    );
    assert_eq!(
        IssueBody::new(&"\r\n".repeat(32768))
            .expect("bound after normalization")
            .as_str(),
        "\n".repeat(32768)
    );
    assert_eq!(IssueBody::new("a\rb"), Err(IssueValueError));
    assert_eq!(IssueBody::new(&"\r\n".repeat(32769)), Err(IssueValueError));
}

#[test]
fn issue_urls_are_canonical_and_cannot_select_another_origin() {
    let repo = RepoName::new("a/b").expect("repo");
    assert_eq!(
        IssueUrl::for_issue(&repo, 42).expect("url").as_str(),
        "https://github.com/a/b/issues/42"
    );
    assert_eq!(IssueUrl::for_issue(&repo, 0), Err(IssueValueError));
    checked::<IssueUrl>(
        IssueUrl::new,
        &[
            "https://github.com/a/b/issues/1".into(),
            format!(
                "https://github.com/{}/issues/{}",
                longest_repo().as_str(),
                u64::MAX
            ),
        ],
        &[
            String::new(),
            "http://github.com/a/b/issues/1".into(),
            "https://evil.test/a/b/issues/1".into(),
            "https://github.com@evil.test/a/b/issues/1".into(),
            "https://github.com/a/b/issues/0".into(),
            "https://github.com/a/b/issues/01".into(),
            "https://github.com/a/b/issues/+1".into(),
            "https://github.com/a/b/issues/18446744073709551616".into(),
            "https://github.com/a/b/issues/1?x".into(),
            "https://github.com/a/b/issues/1#x".into(),
            "https://github.com/a/b/issues/1/".into(),
            "https://github.com/a/b/c/issues/1".into(),
        ],
    );
}

fn longest_repo() -> RepoName {
    RepoName::new(&format!("{}/{}", "a".repeat(39), "b".repeat(100))).expect("max repo")
}

fn card() -> IssueCard {
    IssueCard {
        draft: DraftId(u64::MAX),
        thread: ThreadId(u64::MAX),
        repo: Some(longest_repo()),
        account: Some(GitHubLogin::new(&"a".repeat(39)).expect("login")),
        title: IssueTitle::new(&"🦀".repeat(256)).expect("title"),
        body: IssueBody::new(&"a".repeat(32768)).expect("body"),
        hash: ContentHash([255; 32]),
        approval: Some(ApprovalRev(u64::MAX)),
        version: StateVersion(u64::MAX),
        state: ActionState::Unknown {
            lookup: LookupResult::Unplaced {
                number: u64::MAX,
                url: IssueUrl::for_issue(&longest_repo(), u64::MAX).expect("url"),
            },
            maybe_duplicate: true,
        },
    }
}

fn roundtrip<T: Serialize + DeserializeOwned + std::fmt::Debug + PartialEq>(
    value: &T,
    pipe: Pipe,
    len: usize,
) {
    let bytes = encode(value, pipe).expect("fits cap");
    assert_eq!(bytes.len(), len);
    assert_eq!(
        postcard::from_bytes::<T>(&bytes[HEADER_LEN..]).expect("decode"),
        *value
    );
}

#[test]
fn largest_issue_and_connection_frames_fit_their_exact_budgets() {
    let card = card();
    roundtrip(
        &ControlToShell::Welcome {
            incarnation: Incarnation(u64::MAX),
            issue_epoch: u64::MAX,
        },
        Pipe::Control,
        25,
    );
    roundtrip(
        &ControlToBrain::Issue {
            session: IssueSession {
                incarnation: Incarnation(u64::MAX),
                epoch: u64::MAX,
            },
            request: u64::MAX,
            op: IssueOp::Consent {
                draft: card.draft,
                hash: card.hash,
                approval: card.approval.unwrap(),
                decision: Decision::File,
            },
        },
        Pipe::Control,
        89,
    );
    roundtrip(
        &ControlToShell::GitHub(GitHubView::Connected {
            login: card.account.clone().expect("account"),
            repos: Bounded::new(vec![longest_repo(); 200]).expect("200 repos"),
            truncated: true,
            default: card.repo.clone(),
        }),
        Pipe::Control,
        28592,
    );
    roundtrip(
        &ControlToShell::ActionState {
            request: Some(u64::MAX),
            draft: card.draft,
            version: card.version,
            state: card.state.clone(),
            approval: card.approval,
        },
        Pipe::Control,
        249,
    );
    roundtrip(
        &ControlToShell::IssueRefused {
            request: u64::MAX,
            draft: card.draft,
            version: card.version,
            approval: card.approval,
            reason: IssueRefused::UnresolvedDraft {
                draft: DraftId(u64::MAX),
            },
        },
        Pipe::Control,
        57,
    );
    roundtrip(
        &ControlToShell::IssueUnavailable {
            request: u64::MAX,
            draft: DraftId(u64::MAX),
            kind: IssueUnavailableKind::AttemptNotSaved,
        },
        Pipe::Control,
        26,
    );
    roundtrip(&DataToShell::IssueCard(card.clone()), Pipe::Data, 34261);
    roundtrip(
        &DataToBrain::IssueEdit {
            draft: card.draft,
            title: card.title,
            body: card.body,
            repo: card.repo,
        },
        Pipe::Data,
        33955,
    );
}

#[test]
fn largest_review_frames_fit_their_exact_budgets() {
    let card = card();
    roundtrip(
        &DataToShell::IssueReview {
            request: u64::MAX,
            card: card.clone(),
        },
        Pipe::Data,
        34271,
    );
    for (op, length) in [
        (
            IssueOp::Rearm {
                draft: card.draft,
                version: card.version,
                hash: card.hash,
            },
            88,
        ),
        (
            IssueOp::CancelReview {
                draft: card.draft,
                review: u64::MAX,
            },
            56,
        ),
    ] {
        roundtrip(
            &ControlToBrain::Issue {
                session: IssueSession {
                    incarnation: Incarnation(u64::MAX),
                    epoch: u64::MAX,
                },
                request: u64::MAX,
                op,
            },
            Pipe::Control,
            length,
        );
    }
}

#[test]
fn issue_provenance_round_trips_exactly_through_welcome_and_operation_prefixes() {
    use crate::frame::{FrameError, read};
    for (session, bytes) in [
        (
            IssueSession {
                incarnation: Incarnation(0),
                epoch: 0,
            },
            vec![0, 0],
        ),
        (
            IssueSession {
                incarnation: Incarnation(5),
                epoch: 128,
            },
            vec![5, 128, 1],
        ),
        (
            IssueSession {
                incarnation: Incarnation(u64::MAX),
                epoch: u64::MAX,
            },
            vec![
                255, 255, 255, 255, 255, 255, 255, 255, 255, 1, 255, 255, 255, 255, 255, 255, 255,
                255, 255, 1,
            ],
        ),
    ] {
        assert_eq!(postcard::to_allocvec(&session).unwrap(), bytes);
        assert_eq!(
            postcard::from_bytes::<IssueSession>(&bytes).unwrap(),
            session
        );
        let json = serde_json::json!({"incarnation":session.incarnation.0,"epoch":session.epoch});
        assert_eq!(serde_json::to_value(session).unwrap(), json);
        assert_eq!(
            serde_json::from_value::<IssueSession>(json).unwrap(),
            session
        );
        for length in 0..bytes.len() {
            assert!(postcard::from_bytes::<IssueSession>(&bytes[..length]).is_err());
        }
        let welcome = ControlToShell::Welcome {
            incarnation: session.incarnation,
            issue_epoch: session.epoch,
        };
        let wire = encode(&welcome, Pipe::Control).unwrap();
        let mut payload = vec![0];
        payload.extend(&bytes);
        assert_eq!(&wire[HEADER_LEN..], payload);
        assert_eq!(
            read::<ControlToShell>(&mut wire.as_slice(), Pipe::Control).unwrap(),
            Some(welcome.clone())
        );
        let json = serde_json::json!({"Welcome":{"incarnation":session.incarnation.0,"issue_epoch":session.epoch}});
        assert_eq!(serde_json::to_value(&welcome).unwrap(), json);
        assert_eq!(
            serde_json::from_value::<ControlToShell>(json).unwrap(),
            welcome
        );
        for length in 1..wire.len() {
            assert!(matches!(
                read::<ControlToShell>(&mut &wire[..length], Pipe::Control),
                Err(FrameError::Truncated)
            ));
        }
        let issue = ControlToBrain::Issue {
            session,
            request: 0,
            op: IssueOp::Discard { draft: DraftId(9) },
        };
        let wire = encode(&issue, Pipe::Control).unwrap();
        let mut payload = vec![25];
        payload.extend(bytes);
        payload.extend([0, 1, 9]);
        assert_eq!(&wire[HEADER_LEN..], payload);
        assert_eq!(
            read::<ControlToBrain>(&mut wire.as_slice(), Pipe::Control).unwrap(),
            Some(issue)
        );
        for length in 1..wire.len() {
            assert!(matches!(
                read::<ControlToBrain>(&mut &wire[..length], Pipe::Control),
                Err(FrameError::Truncated)
            ));
        }
    }
}

#[test]
fn issue_provenance_rejects_missing_mistyped_overflow_and_truncated_identities() {
    for json in [
        "{}",
        r#"{"incarnation":1}"#,
        r#"{"epoch":2}"#,
        r#"{"incarnation":"1","epoch":2}"#,
        r#"{"incarnation":18446744073709551616,"epoch":2}"#,
        r#"{"incarnation":1,"epoch":18446744073709551616}"#,
    ] {
        assert!(serde_json::from_str::<IssueSession>(json).is_err());
    }
    for identity in [
        vec![128, 128, 128, 128, 128, 128, 128, 128, 128, 2, 0],
        vec![0, 128, 128, 128, 128, 128, 128, 128, 128, 128, 2],
    ] {
        assert!(postcard::from_bytes::<IssueSession>(&identity).is_err());
        let mut welcome = vec![0];
        welcome.extend(&identity);
        assert!(postcard::from_bytes::<ControlToShell>(&welcome).is_err());
        let mut issue = vec![25];
        issue.extend(identity);
        issue.extend([0, 1, 9]);
        assert!(postcard::from_bytes::<ControlToBrain>(&issue).is_err());
    }
    assert!(serde_json::from_str::<ControlToShell>(r#"{"Welcome":{"incarnation":1}}"#).is_err());
    assert!(
        serde_json::from_str::<ControlToBrain>(
            r#"{"Issue":{"request":0,"op":{"Discard":{"draft":9}}}}"#
        )
        .is_err()
    );
}

#[test]
fn issue_operation_identity_preserves_inner_tags_and_exact_wire_fields() {
    use crate::frame::{FrameError, read};
    let mut consent = vec![0, 2];
    consent.extend([91; 32]);
    consent.extend([3, 0]);
    let mut rearm = vec![3, 2, 3];
    rearm.extend([91; 32]);
    for (op, inner) in [
        (
            IssueOp::Consent {
                draft: DraftId(2),
                hash: ContentHash([91; 32]),
                approval: ApprovalRev(3),
                decision: Decision::File,
            },
            consent,
        ),
        (IssueOp::Discard { draft: DraftId(2) }, vec![1, 2]),
        (IssueOp::CheckAgain { draft: DraftId(2) }, vec![2, 2]),
        (
            IssueOp::Rearm {
                draft: DraftId(2),
                version: StateVersion(3),
                hash: ContentHash([91; 32]),
            },
            rearm,
        ),
        (
            IssueOp::CancelReview {
                draft: DraftId(2),
                review: 128,
            },
            vec![4, 2, 128, 1],
        ),
    ] {
        for (request, identity) in [
            (0, vec![0]),
            (
                u64::MAX,
                vec![255, 255, 255, 255, 255, 255, 255, 255, 255, 1],
            ),
        ] {
            let message = ControlToBrain::Issue {
                session: IssueSession {
                    incarnation: Incarnation(1),
                    epoch: 2,
                },
                request,
                op: op.clone(),
            };
            let mut expected = vec![25, 1, 2];
            expected.extend(identity);
            expected.extend(&inner);
            let wire = encode(&message, Pipe::Control).unwrap();
            assert_eq!(&wire[HEADER_LEN..], expected);
            assert_eq!(
                read::<ControlToBrain>(&mut wire.as_slice(), Pipe::Control).unwrap(),
                Some(message.clone())
            );
            assert_eq!(
                serde_json::from_value::<ControlToBrain>(serde_json::to_value(&message).unwrap())
                    .unwrap(),
                message
            );
            for length in 1..wire.len() {
                assert!(matches!(
                    read::<ControlToBrain>(&mut &wire[..length], Pipe::Control),
                    Err(FrameError::Truncated)
                ));
            }
            assert!(!format!("{message:?}").contains(&format!("{:?}", [91; 32])));
        }
    }
    assert_eq!(
        serde_json::to_value(ControlToBrain::Issue {
            session: IssueSession {
                incarnation: Incarnation(1),
                epoch: 2
            },
            request: 128,
            op: IssueOp::Discard { draft: DraftId(2) }
        })
        .unwrap(),
        serde_json::json!({"Issue":{"session":{"incarnation":1,"epoch":2},"request":128,"op":{"Discard":{"draft":2}}}})
    );
}

#[test]
fn issue_operation_unknown_tags_and_overflow_are_rejected_without_changing_decisions() {
    for decision in [Decision::FileAnyway, Decision::ThisIsTheIssue] {
        let message = ControlToBrain::Issue {
            session: IssueSession {
                incarnation: Incarnation(1),
                epoch: 2,
            },
            request: 1,
            op: IssueOp::Consent {
                draft: DraftId(2),
                hash: ContentHash([91; 32]),
                approval: ApprovalRev(3),
                decision,
            },
        };
        assert_eq!(
            postcard::from_bytes::<ControlToBrain>(&postcard::to_allocvec(&message).unwrap())
                .unwrap(),
            message
        );
    }
    assert!(postcard::from_bytes::<ControlToBrain>(&[25, 1, 2, 0, 5]).is_err());
    let mut unknown_decision = vec![25, 1, 2, 0, 0, 2];
    unknown_decision.extend([91; 32]);
    unknown_decision.extend([3, 3]);
    assert!(postcard::from_bytes::<ControlToBrain>(&unknown_decision).is_err());
    let mut overflow = vec![25, 1, 2];
    overflow.extend([128; 10]);
    overflow.extend([1, 2]);
    assert!(postcard::from_bytes::<ControlToBrain>(&overflow).is_err());
    assert!(
        serde_json::from_str::<ControlToBrain>(
            r#"{"Issue":{"session":{"incarnation":1,"epoch":2},"request":18446744073709551616,"op":{"Discard":{"draft":2}}}}"#
        )
        .is_err()
    );
    assert!(
        serde_json::from_str::<ControlToBrain>(
            r#"{"Issue":{"session":{"incarnation":1,"epoch":2},"request":1,"op":"Unknown"}}"#
        )
        .is_err()
    );
}

#[test]
fn action_acknowledgements_keep_exact_authoritative_metadata() {
    use crate::frame::{FrameError, read};
    for (request, identity) in [(None, vec![0]), (Some(128), vec![1, 128, 1])] {
        let message = ControlToShell::ActionState {
            request,
            draft: DraftId(2),
            version: StateVersion(3),
            state: ActionState::Draft,
            approval: Some(ApprovalRev(4)),
        };
        let mut expected = vec![18];
        expected.extend(identity);
        expected.extend([2, 3, 0, 1, 4]);
        let wire = encode(&message, Pipe::Control).unwrap();
        assert_eq!(&wire[HEADER_LEN..], expected);
        assert_eq!(
            read::<ControlToShell>(&mut wire.as_slice(), Pipe::Control).unwrap(),
            Some(message.clone())
        );
        assert_eq!(
            serde_json::from_value::<ControlToShell>(serde_json::to_value(&message).unwrap())
                .unwrap(),
            message
        );
        for length in 1..wire.len() {
            assert!(matches!(
                read::<ControlToShell>(&mut &wire[..length], Pipe::Control),
                Err(FrameError::Truncated)
            ));
        }
    }
    assert!(postcard::from_bytes::<ControlToShell>(&[18, 2]).is_err());
}

#[test]
fn issue_refusals_keep_exact_authoritative_metadata() {
    use crate::frame::{FrameError, read};
    for (approval, ending) in [(None, vec![0, 5]), (Some(ApprovalRev(4)), vec![1, 4, 5])] {
        let message = ControlToShell::IssueRefused {
            request: 128,
            draft: DraftId(2),
            version: StateVersion(3),
            approval,
            reason: IssueRefused::NotAllowedNow,
        };
        let mut expected = vec![19, 128, 1, 2, 3];
        expected.extend(ending);
        let wire = encode(&message, Pipe::Control).unwrap();
        assert_eq!(&wire[HEADER_LEN..], expected);
        assert_eq!(
            read::<ControlToShell>(&mut wire.as_slice(), Pipe::Control).unwrap(),
            Some(message.clone())
        );
        assert_eq!(
            serde_json::from_value::<ControlToShell>(serde_json::to_value(&message).unwrap())
                .unwrap(),
            message
        );
        for length in 1..wire.len() {
            assert!(matches!(
                read::<ControlToShell>(&mut &wire[..length], Pipe::Control),
                Err(FrameError::Truncated)
            ));
        }
        let mut unknown = wire;
        *unknown.last_mut().unwrap() = 8;
        assert!(matches!(
            read::<ControlToShell>(&mut unknown.as_slice(), Pipe::Control),
            Err(FrameError::Malformed(_))
        ));
    }
    assert!(postcard::from_bytes::<ControlToShell>(&[19, 1, 2, 3, 2]).is_err());
    assert_eq!(
        serde_json::to_value(ControlToShell::IssueRefused {
            request: 128,
            draft: DraftId(2),
            version: StateVersion(3),
            approval: Some(ApprovalRev(4)),
            reason: IssueRefused::NotAllowedNow,
        })
        .unwrap(),
        serde_json::json!({"IssueRefused":{"request":128,"draft":2,"version":3,"approval":4,"reason":"NotAllowedNow"}})
    );
}

#[test]
fn issue_unavailable_has_only_correlated_identity_and_round_trips_exact_wire_data() {
    use crate::frame::{FrameError, read};
    for (request, draft, mut payload) in [
        (0, DraftId(0), vec![26, 0, 0]),
        (128, DraftId(2), vec![26, 128, 1, 2]),
        (
            u64::MAX,
            DraftId(u64::MAX),
            vec![
                26, 255, 255, 255, 255, 255, 255, 255, 255, 255, 1, 255, 255, 255, 255, 255, 255,
                255, 255, 255, 1,
            ],
        ),
    ] {
        payload.push(0);
        let message = ControlToShell::IssueUnavailable {
            request,
            draft,
            kind: IssueUnavailableKind::Action,
        };
        let wire = encode(&message, Pipe::Control).unwrap();
        assert_eq!(&wire[HEADER_LEN..], payload);
        assert_eq!(
            read::<ControlToShell>(&mut wire.as_slice(), Pipe::Control).unwrap(),
            Some(message.clone())
        );
        let json = serde_json::json!({"IssueUnavailable":{"request":request,"draft":draft.0,"kind":"Action"}});
        assert_eq!(serde_json::to_value(&message).unwrap(), json);
        assert_eq!(
            serde_json::from_value::<ControlToShell>(json).unwrap(),
            message
        );
        for length in 1..wire.len() {
            assert!(matches!(
                read::<ControlToShell>(&mut &wire[..length], Pipe::Control),
                Err(FrameError::Truncated)
            ));
        }
        for length in 0..payload.len() {
            assert!(postcard::from_bytes::<ControlToShell>(&payload[..length]).is_err());
        }
        let record =
            Record::of(Pipe::Control, Direction::ToShell, &message, payload.len()).unwrap();
        assert_eq!(record.payload, None);
        assert_eq!(
            record.describe(),
            format!("IssueUnavailable ({} bytes)", payload.len())
        );
    }
    assert_eq!(
        format!(
            "{:?}",
            ControlToShell::IssueUnavailable {
                request: 128,
                draft: DraftId(2),
                kind: IssueUnavailableKind::Action,
            }
        ),
        "IssueUnavailable { request: 128, draft: DraftId(2), kind: Action }"
    );
}

#[test]
fn issue_unavailable_rejects_unknown_tags_overflow_missing_and_mistyped_identity() {
    use crate::frame::{FrameError, read};
    for payload in [
        vec![27],
        vec![26, 128, 128, 128, 128, 128, 128, 128, 128, 128, 2, 0],
        vec![26, 0, 128, 128, 128, 128, 128, 128, 128, 128, 128, 2],
        vec![26, 1, 2, 4],
    ] {
        assert!(postcard::from_bytes::<ControlToShell>(&payload).is_err());
        let mut wire = u32::try_from(payload.len()).unwrap().to_le_bytes().to_vec();
        wire.extend(payload);
        assert!(matches!(
            read::<ControlToShell>(&mut wire.as_slice(), Pipe::Control),
            Err(FrameError::Malformed(_))
        ));
    }
    for json in [
        r#"{"IssueUnavailable":{"draft":2,"kind":"Action"}}"#,
        r#"{"IssueUnavailable":{"request":1,"kind":"Action"}}"#,
        r#"{"IssueUnavailable":{"request":18446744073709551616,"draft":2,"kind":"Action"}}"#,
        r#"{"IssueUnavailable":{"request":1,"draft":18446744073709551616,"kind":"Action"}}"#,
        r#"{"IssueUnavailable":{"request":"1","draft":2,"kind":"Action"}}"#,
        r#"{"IssueUnavailable":{"request":1,"draft":2}}"#,
        r#"{"IssueUnavailable":{"request":1,"draft":2,"kind":"Unknown"}}"#,
    ] {
        assert!(serde_json::from_str::<ControlToShell>(json).is_err());
    }
}

#[test]
fn bounded_collections_and_hash_reject_excess_or_truncated_wire_data() {
    for count in [0, 16] {
        let drafts = vec![DraftId(7); count];
        let bounded = Bounded::<DraftId, MAX_ISSUE_CARDS>::new(drafts.clone()).expect("bound");
        assert_eq!(
            &*postcard::from_bytes::<Bounded<DraftId, MAX_ISSUE_CARDS>>(
                &postcard::to_allocvec(&drafts).expect("raw")
            )
            .expect("decode"),
            drafts.as_slice()
        );
        let request = DataToBrain::IssueCards { drafts: bounded };
        assert_eq!(
            postcard::from_bytes::<DataToBrain>(&postcard::to_allocvec(&request).expect("encode"))
                .expect("decode"),
            request
        );
    }
    assert!(Bounded::<DraftId, MAX_ISSUE_CARDS>::new(vec![DraftId(7); 17]).is_err());
    assert!(
        postcard::from_bytes::<Bounded<DraftId, MAX_ISSUE_CARDS>>(
            &postcard::to_allocvec(&vec![DraftId(7); 17]).expect("raw")
        )
        .is_err()
    );
    assert!(Bounded::<RepoName, MAX_GITHUB_REPOS>::new(vec![longest_repo(); 201]).is_err());
    assert!(
        postcard::from_bytes::<Bounded<RepoName, MAX_GITHUB_REPOS>>(
            &postcard::to_allocvec(&vec![longest_repo(); 201]).expect("raw")
        )
        .is_err()
    );
    let hash = ContentHash([91; 32]);
    assert_eq!(postcard::to_allocvec(&hash).expect("bytes"), vec![91; 32]);
    assert_eq!(
        postcard::from_bytes::<ContentHash>(&[91; 32]).expect("hash"),
        hash
    );
    assert!(postcard::from_bytes::<ContentHash>(&[91; 31]).is_err());
    assert!(serde_json::from_str::<ContentHash>("[1]").is_err());
}

fn private(
    message: &(impl Tapped + std::fmt::Debug),
    pipe: Pipe,
    direction: Direction,
    markers: &[&str],
) {
    let wire = encode(message, pipe).expect("wire");
    let record = Record::of(pipe, direction, message, wire.len() - HEADER_LEN).expect("record");
    assert_eq!(record.payload, None);
    assert_eq!(
        record.describe(),
        format!("{} ({} bytes)", message.op(), wire.len() - HEADER_LEN)
    );
    let mut bytes = Vec::new();
    write(&mut bytes, &record).expect("write");
    let debug = format!("{message:?} {record:?}");
    for marker in markers {
        assert!(!debug.contains(marker), "debug leaked content");
        assert!(
            !debug.contains(&format!("{:?}", marker.as_bytes())),
            "debug leaked byte list"
        );
        assert!(
            !bytes
                .windows(marker.len())
                .any(|part| part == marker.as_bytes()),
            "tap leaked content"
        );
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one sentinel scenario follows private content through every outer wire carrier and tap"
)]
fn outer_wire_carriers_and_tap_never_format_issue_or_device_content() {
    let title = "zqx-private-title";
    let body = "zqx-private-body";
    let login = "zqx-private-login";
    let repo = "zqx-private-owner/zqx-private-repo";
    let code = "ZQX-PRIVATE-CODE";
    let markers = [title, body, login, repo, code];
    let mut card = card();
    card.title = IssueTitle::new(title).expect("title");
    card.body = IssueBody::new(body).expect("body");
    card.repo = Some(RepoName::new(repo).expect("repo"));
    card.account = Some(GitHubLogin::new(login).expect("login"));
    card.state = ActionState::Unknown {
        lookup: LookupResult::Unplaced {
            number: 42,
            url: IssueUrl::for_issue(card.repo.as_ref().expect("repo"), 42).expect("url"),
        },
        maybe_duplicate: false,
    };
    private(
        &ControlToShell::GitHub(GitHubView::AwaitingUser {
            user_code: UserCode::new(code).expect("code"),
            expires_in_s: 900,
        }),
        Pipe::Control,
        Direction::ToShell,
        &markers,
    );
    private(
        &ControlToShell::GitHub(GitHubView::Connected {
            login: card.account.clone().expect("account"),
            repos: Bounded::new(vec![card.repo.clone().expect("repo")]).expect("repos"),
            truncated: false,
            default: card.repo.clone(),
        }),
        Pipe::Control,
        Direction::ToShell,
        &markers,
    );
    private(
        &ControlToBrain::GitHub(GitHubOp::SetDefaultRepo(card.repo.clone().expect("repo"))),
        Pipe::Control,
        Direction::ToBrain,
        &markers,
    );
    private(
        &ControlToBrain::Issue {
            session: IssueSession {
                incarnation: Incarnation(u64::MAX),
                epoch: u64::MAX,
            },
            request: u64::MAX,
            op: IssueOp::Consent {
                draft: card.draft,
                hash: card.hash,
                approval: card.approval.expect("approval"),
                decision: Decision::File,
            },
        },
        Pipe::Control,
        Direction::ToBrain,
        &markers,
    );
    private(
        &ControlToShell::ActionState {
            request: Some(u64::MAX),
            draft: card.draft,
            version: card.version,
            state: card.state.clone(),
            approval: card.approval,
        },
        Pipe::Control,
        Direction::ToShell,
        &markers,
    );
    private(
        &ControlToShell::IssueRefused {
            request: u64::MAX,
            draft: card.draft,
            version: card.version,
            approval: card.approval,
            reason: IssueRefused::ContentChanged,
        },
        Pipe::Control,
        Direction::ToShell,
        &markers,
    );
    private(
        &DataToBrain::IssueEdit {
            draft: card.draft,
            title: card.title.clone(),
            body: card.body.clone(),
            repo: card.repo.clone(),
        },
        Pipe::Data,
        Direction::ToBrain,
        &markers,
    );
    private(
        &DataToShell::IssueReview {
            request: u64::MAX,
            card: card.clone(),
        },
        Pipe::Data,
        Direction::ToShell,
        &markers,
    );
    private(
        &DataToShell::IssueCard(card),
        Pipe::Data,
        Direction::ToShell,
        &markers,
    );
    assert_eq!(format!("{:?}", ContentHash([91; 32])), "ContentHash(..)");
}

#[test]
fn issue_page_debug_omits_its_title() {
    assert_eq!(
        format!(
            "{:?}",
            PageEvent {
                id: EventId(1),
                run: None,
                kind: PageEventKind::Issue { draft: DraftId(2) },
                text: "zqx-private-title".into(),
                cut: false
            }
        ),
        "PageEvent { id: EventId(1), run: None, kind: Issue { draft: DraftId(2) }, cut: false, .. }"
    );
}

#[test]
fn review_operations_have_closed_json_shapes_and_numeric_bounds() {
    assert_eq!(MAX_ISSUE_REVIEWS, 16);
    for (op, json) in [
        (
            IssueOp::Rearm {
                draft: DraftId(2),
                version: StateVersion(3),
                hash: ContentHash([7; 32]),
            },
            serde_json::json!({"Rearm":{"draft":2,"version":3,"hash":vec![7;32]}}),
        ),
        (
            IssueOp::CancelReview {
                draft: DraftId(2),
                review: 128,
            },
            serde_json::json!({"CancelReview":{"draft":2,"review":128}}),
        ),
    ] {
        assert_eq!(serde_json::to_value(&op).unwrap(), json);
        assert_eq!(serde_json::from_value::<IssueOp>(json.clone()).unwrap(), op);
        let name = if matches!(op, IssueOp::Rearm { .. }) {
            "Rearm"
        } else {
            "CancelReview"
        };
        let fields = if name == "Rearm" {
            vec!["draft", "version", "hash"]
        } else {
            vec!["draft", "review"]
        };
        for field in fields {
            let mut missing = json.clone();
            missing[name].as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<IssueOp>(missing).is_err());
            for invalid in [serde_json::json!("wrong"), serde_json::json!(-1)] {
                let mut malformed = json.clone();
                malformed[name][field] = invalid;
                assert!(serde_json::from_value::<IssueOp>(malformed).is_err());
            }
        }
    }
    for json in [
        r#"{"Rearm":{"draft":2,"version":18446744073709551616,"hash":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}}"#,
        r#"{"CancelReview":{"draft":2,"review":18446744073709551616}}"#,
        r#"{"CancelReview":{"draft":18446744073709551616,"review":1}}"#,
    ] {
        assert!(serde_json::from_str::<IssueOp>(json).is_err());
    }
    for tag in [3, 4] {
        let mut malformed = vec![tag, 2];
        malformed.extend([128; 10]);
        malformed.extend([7; 32]);
        assert!(postcard::from_bytes::<IssueOp>(&malformed).is_err());
    }
}

#[test]
fn review_data_keeps_its_request_full_card_and_exact_appended_wire_tag() {
    use crate::frame::{FrameError, read};
    let card = IssueCard {
        draft: DraftId(1),
        thread: ThreadId(2),
        repo: None,
        account: None,
        title: IssueTitle::new("t").unwrap(),
        body: IssueBody::new("b").unwrap(),
        hash: ContentHash([0; 32]),
        approval: Some(ApprovalRev(3)),
        version: StateVersion(4),
        state: ActionState::Draft,
    };
    for (request, identity) in [
        (0, vec![0]),
        (
            u64::MAX,
            vec![255, 255, 255, 255, 255, 255, 255, 255, 255, 1],
        ),
    ] {
        let mut payload = vec![20];
        payload.extend(identity);
        payload.extend([1, 2, 0, 0, 1, b't', 1, b'b']);
        payload.extend([0; 32]);
        payload.extend([1, 3, 4, 0]);
        let message = DataToShell::IssueReview {
            request,
            card: card.clone(),
        };
        let wire = encode(&message, Pipe::Data).unwrap();
        assert_eq!(&wire[HEADER_LEN..], payload);
        assert_eq!(
            read::<DataToShell>(&mut wire.as_slice(), Pipe::Data).unwrap(),
            Some(message.clone())
        );
        let json = serde_json::json!({"IssueReview":{"request":request,"card":{
            "draft":1,"thread":2,"repo":null,"account":null,"title":"t","body":"b",
            "hash":vec![0;32],"approval":3,"version":4,"state":"Draft"
        }}});
        assert_eq!(serde_json::to_value(&message).unwrap(), json);
        assert_eq!(
            serde_json::from_value::<DataToShell>(json.clone()).unwrap(),
            message
        );
        for length in 1..wire.len() {
            assert!(matches!(
                read::<DataToShell>(&mut &wire[..length], Pipe::Data),
                Err(FrameError::Truncated)
            ));
        }
        for length in 0..payload.len() {
            assert!(postcard::from_bytes::<DataToShell>(&payload[..length]).is_err());
        }
        for (field, value) in [
            ("title", serde_json::json!("a".repeat(257))),
            ("body", serde_json::json!("a".repeat(32769))),
            ("hash", serde_json::json!(vec![0; 31])),
        ] {
            let mut invalid = json.clone();
            invalid["IssueReview"]["card"][field] = value;
            assert!(serde_json::from_value::<DataToShell>(invalid).is_err());
        }
        for field in ["request", "card"] {
            let mut missing = json.clone();
            missing["IssueReview"]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(serde_json::from_value::<DataToShell>(missing).is_err());
        }
    }
    assert!(postcard::from_bytes::<DataToShell>(&[21]).is_err());
    assert!(
        postcard::from_bytes::<DataToShell>(&[20, 128, 128, 128, 128, 128, 128, 128, 128, 128, 2])
            .is_err()
    );
}

#[test]
fn unavailable_kinds_are_closed_and_keep_distinct_wire_and_json_tags() {
    for (kind, tag, name) in [
        (IssueUnavailableKind::Action, 0, "Action"),
        (IssueUnavailableKind::ReviewNotSaved, 1, "ReviewNotSaved"),
        (IssueUnavailableKind::ReviewCapacity, 2, "ReviewCapacity"),
        (IssueUnavailableKind::AttemptNotSaved, 3, "AttemptNotSaved"),
    ] {
        assert_eq!(postcard::to_allocvec(&kind).unwrap(), [tag]);
        assert_eq!(
            postcard::from_bytes::<IssueUnavailableKind>(&[tag]).unwrap(),
            kind
        );
        let message = ControlToShell::IssueUnavailable {
            request: 1,
            draft: DraftId(2),
            kind,
        };
        let wire = encode(&message, Pipe::Control).unwrap();
        assert_eq!(wire, [4, 0, 0, 0, 26, 1, 2, tag]);
        assert_eq!(
            serde_json::to_value(&message).unwrap(),
            serde_json::json!({"IssueUnavailable":{"request":1,"draft":2,"kind":name}})
        );
        assert_eq!(
            serde_json::from_value::<IssueUnavailableKind>(serde_json::json!(name)).unwrap(),
            kind
        );
    }
    assert!(postcard::from_bytes::<IssueUnavailableKind>(&[4]).is_err());
    assert!(postcard::from_bytes::<IssueUnavailableKind>(&[]).is_err());
}

#[test]
fn github_signed_out_cleanup_has_exact_optional_wire_forms() {
    for (cleanup, bytes) in [
        (None, vec![0, 0]),
        (Some(Cleanup::Running), vec![0, 1, 0]),
        (Some(Cleanup::Refused), vec![0, 1, 1]),
        (Some(Cleanup::NotDurable), vec![0, 1, 2]),
    ] {
        let view = GitHubView::SignedOut { cleanup };
        assert_eq!(postcard::to_stdvec(&view).expect("encode"), bytes);
        assert_eq!(
            postcard::from_bytes::<GitHubView>(&bytes).expect("decode"),
            view
        );
        let json = serde_json::to_value(&view).expect("json");
        assert_eq!(
            serde_json::from_value::<GitHubView>(json).expect("json decode"),
            view
        );
    }
    assert_eq!(
        serde_json::from_str::<GitHubView>(r#"{"SignedOut":{}}"#)
            .expect("missing optional cleanup"),
        GitHubView::SignedOut { cleanup: None }
    );
}
