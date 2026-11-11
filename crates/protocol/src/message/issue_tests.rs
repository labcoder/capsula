use kumochi_scene::text::Bounded;
use serde::{Serialize, de::DeserializeOwned};

use super::*;
use crate::frame::{HEADER_LEN, Pipe, encode};
use crate::tap::{Direction, Record, Tapped, write};

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
    let mut unknown = wire;
    unknown[5] = 43;
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
            draft: card.draft,
            version: card.version,
            state: card.state.clone(),
            approval: card.approval,
        },
        Pipe::Control,
        238,
    );
    roundtrip(
        &ControlToShell::IssueRefused {
            draft: card.draft,
            reason: IssueRefused::ThreadGone,
        },
        Pipe::Control,
        16,
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
        &ControlToBrain::Issue(IssueOp::Consent {
            draft: card.draft,
            hash: card.hash,
            approval: card.approval.expect("approval"),
            decision: Decision::File,
        }),
        Pipe::Control,
        Direction::ToBrain,
        &markers,
    );
    private(
        &ControlToShell::ActionState {
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
            draft: card.draft,
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
