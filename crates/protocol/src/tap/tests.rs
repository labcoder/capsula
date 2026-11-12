use super::{Direction, MAGIC, Record, Tapped, dump, read, read_header, write, write_header};
use crate::Secret;
use crate::frame::{FrameError, HEADER_LEN, Pipe, encode};
use crate::message::{
    AccountEmail, AccountOp, AccountView, AssistantName, ClaudeCodeView, ClaudeVersion,
    ConnectionOp, ConnectionsView, ControlToBrain, ControlToShell, DataToBrain, Hello, Incarnation,
    KeyView, PlanState, SettingsOp, SettingsView, ThreadId, ThreadOp, ThreadTitle, UserName,
};

const NAME: &str = "Zqxassistant";
const USER: &str = "Zqxuser";

#[test]
fn drawing_failure_controls_record_only_their_closed_kinds() {
    use crate::message::{RunId, SceneId};
    let failed = ControlToBrain::SceneFailed { scene: SceneId(9) };
    let notice = ControlToShell::DrawingReplacementFailed {
        thread: ThreadId(7),
        run: RunId(3),
    };
    let nothing = ControlToShell::DrawingNotReturned {
        thread: ThreadId(7),
        run: RunId(3),
    };
    let failed = record(Direction::ToBrain, &failed);
    let notice = record(Direction::ToShell, &notice);
    let nothing = record(Direction::ToShell, &nothing);
    assert_eq!((failed.op.as_str(), failed.payload), ("SceneFailed", None));
    assert_eq!(
        (notice.op.as_str(), notice.payload),
        ("DrawingReplacementFailed", None)
    );
    assert_eq!(
        (nothing.op.as_str(), nothing.payload),
        ("DrawingNotReturned", None)
    );
}

#[test]
fn github_refusal_is_content_free_while_its_repository_request_remains_redacted() {
    use crate::message::{ErrorKey, GitHubOp, RepoName};
    let marker = "sentinel/private-repository";
    let request = ControlToBrain::GitHub(GitHubOp::SetDefaultRepo(RepoName::new(marker).unwrap()));
    let refusal = ControlToShell::GitHubError(ErrorKey::GitHubUnavailable.into());
    let records = [
        record(Direction::ToBrain, &request),
        record(Direction::ToShell, &refusal),
    ];
    assert_eq!(records[0].payload, None);
    assert_eq!(records[1].payload, Some(vec![25, 42, 0, 0]));
    let file = recording(&records);
    assert!(!contains(&file, marker.as_bytes()));
    let mut reader = &file[MAGIC.len()..];
    assert_eq!(read(&mut reader).unwrap(), Some(records[0].clone()));
    assert_eq!(read(&mut reader).unwrap(), Some(records[1].clone()));
    assert_eq!(read(&mut reader).unwrap(), None);
    let mut out = Vec::new();
    assert_eq!(dump(&mut file.as_slice(), &mut out).unwrap(), 2);
    let out = String::from_utf8(out).unwrap();
    assert_eq!(
        out,
        format!(
            "    0 Control shell -> brain  GitHub ({} bytes)\n    1 Control brain -> shell  GitHubError(UserError {{ key: GitHubUnavailable, args: [], actions: [] }})\n2 frames\n",
            wire_len(&request, Pipe::Control)
        )
    );
    let debug = format!("{request:?} {refusal:?} {records:?} {out}");
    assert!(!debug.contains(marker));
    assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
}

fn wire_len(message: &impl Tapped, pipe: Pipe) -> usize {
    encode(message, pipe).expect("encodes").len() - HEADER_LEN
}

fn record(direction: Direction, message: &impl Tapped) -> Record {
    Record::of(
        Pipe::Control,
        direction,
        message,
        wire_len(message, Pipe::Control),
    )
    .expect("records")
}

fn view() -> ControlToShell {
    ControlToShell::Settings {
        view: SettingsView {
            assistant_name: AssistantName::new(NAME).expect("valid"),
            user_name: Some(UserName::new(USER).expect("valid")),
            onboarded: true,
            main_thread: ThreadId(1),
            max_frame_rate: crate::message::MaxFrameRate::AUTOMATIC,
            default_model: None,
            dock_position: crate::message::DockPosition::TopCenter,
            show_in_taskbar: false,
            talk_back: false,
            stt_unload: crate::message::SttUnload::Kept,
        },
        answer: true,
    }
}

fn recording(records: &[Record]) -> Vec<u8> {
    let mut file = Vec::new();
    write_header(&mut file).expect("header");
    for record in records {
        write(&mut file, record).expect("writes");
    }
    file
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn content_free_ops_keep_their_fields_and_others_only_their_name_and_length() {
    let welcome = ControlToShell::Welcome {
        incarnation: Incarnation(7),
    };
    let hello = ControlToBrain::Hello(Hello {
        build_id: "0.1.0+abc".to_owned(),
        capability: Secret::new([9; 32]),
    });
    let rename = ControlToBrain::Settings(SettingsOp::SetAssistantName(
        AssistantName::new(NAME).expect("valid"),
    ));
    let records = [
        record(Direction::ToBrain, &hello),
        record(Direction::ToShell, &welcome),
        record(Direction::ToBrain, &rename),
        record(Direction::ToShell, &view()),
    ];
    let file = recording(&records);
    for secret in [&[9; 32][..], NAME.as_bytes(), USER.as_bytes(), b"0.1.0+abc"] {
        assert!(!contains(&file, secret), "{secret:?} was recorded");
    }
    let mut out = Vec::new();
    assert_eq!(dump(&mut file.as_slice(), &mut out).expect("dumps"), 4);
    let out = String::from_utf8(out).expect("utf-8");
    let hello_len = wire_len(&hello, Pipe::Control);
    let view_len = wire_len(&view(), Pipe::Control);
    assert_eq!(
        out,
        [
            format!("    0 Control shell -> brain  Hello ({hello_len} bytes)"),
            "    1 Control brain -> shell  Welcome { incarnation: Incarnation(7) }".to_owned(),
            format!(
                "    2 Control shell -> brain  Settings ({} bytes)",
                wire_len(&rename, Pipe::Control)
            ),
            format!("    3 Control brain -> shell  Settings ({view_len} bytes)"),
            "4 frames\n".to_owned(),
        ]
        .join("\n")
    );
    let mut reader = &file[MAGIC.len()..];
    let read_back: Vec<_> = std::iter::from_fn(|| read(&mut reader).expect("reads")).collect();
    assert_eq!(read_back, records);
}

#[test]
fn a_data_pipe_hello_keeps_only_its_op_name_and_length() {
    let capability = *b"Zqxcapabilityzqxcapabilityzqx123";
    let hello = DataToBrain::Hello(Hello {
        build_id: "0.1.0+abc".to_owned(),
        capability: Secret::new(capability),
    });
    let len = wire_len(&hello, Pipe::Data);
    let record = Record::of(Pipe::Data, Direction::ToBrain, &hello, len).expect("records");
    assert_eq!(record.payload, None);
    let file = recording(&[record]);
    for secret in [&capability[..], b"0.1.0+abc"] {
        assert!(!contains(&file, secret), "{secret:?} was recorded");
    }
    let mut out = Vec::new();
    dump(&mut file.as_slice(), &mut out).expect("dumps");
    assert_eq!(
        String::from_utf8(out).expect("utf-8"),
        format!("    0 Data    shell -> brain  Hello ({len} bytes)\n1 frames\n")
    );
}

#[test]
fn a_model_result_round_trips_but_never_records_its_choice() {
    use crate::message::{ConnectionKind, Effort, ModelChoice, ModelSlug};

    const MODEL: &str = "private-model-sentinel";
    for applied in [false, true] {
        let result = ControlToShell::ThreadModelSet {
            thread: ThreadId(8),
            choice: ModelChoice {
                model: ModelSlug::new(MODEL).expect("valid"),
                effort: Effort::XHigh,
                connection: ConnectionKind::ChatGpt,
            },
            applied,
        };
        let bytes = encode(&result, Pipe::Control).expect("encodes");
        assert_eq!(
            crate::frame::read::<ControlToShell>(&mut bytes.as_slice(), Pipe::Control)
                .expect("decodes"),
            Some(result.clone())
        );
        let debug = format!("{result:?}");
        assert!(!debug.contains(MODEL), "{debug}");
        assert!(
            !debug.contains(&format!("{:?}", MODEL.as_bytes())),
            "{debug}"
        );
        let record = record(Direction::ToShell, &result);
        assert_eq!(record.payload, None);
        let file = recording(&[record]);
        assert!(!contains(&file, MODEL.as_bytes()));
        assert!(!format!("{file:?}").contains(&format!("{:?}", MODEL.as_bytes())));
        let mut out = Vec::new();
        dump(&mut file.as_slice(), &mut out).expect("dumps");
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            format!(
                "    0 Control brain -> shell  ThreadModelSet ({} bytes)\n1 frames\n",
                wire_len(&result, Pipe::Control)
            )
        );
    }
}

#[test]
fn a_rename_keeps_only_its_op_name_and_length() {
    const TITLE: &str = "Zqxrenamedtitle";
    let rename = ControlToBrain::ThreadOp(ThreadOp::Rename {
        thread: ThreadId(2),
        title: ThreadTitle::new(TITLE).expect("valid"),
    });
    let record = record(Direction::ToBrain, &rename);
    assert_eq!(record.payload, None);
    let file = recording(&[record]);
    assert!(!contains(&file, TITLE.as_bytes()), "the title was recorded");
    let mut out = Vec::new();
    dump(&mut file.as_slice(), &mut out).expect("dumps");
    assert_eq!(
        String::from_utf8(out).expect("utf-8"),
        format!(
            "    0 Control shell -> brain  ThreadOp ({} bytes)\n1 frames\n",
            wire_len(&rename, Pipe::Control)
        )
    );
}

#[test]
fn an_account_op_keeps_its_fields_and_the_account_view_only_its_name() {
    const EMAIL: &str = "zqxperson@example.invalid";
    let op = ControlToBrain::AccountOp(AccountOp::SignIn);
    let view = ControlToShell::Account(AccountView::SignedIn {
        email: Some(AccountEmail::new(EMAIL).expect("valid")),
        plan: PlanState::Enabled,
        recovery: None,
    });
    let records = [
        record(Direction::ToBrain, &op),
        record(Direction::ToShell, &view),
    ];
    assert!(records[0].payload.is_some(), "the op is content-free");
    assert_eq!(records[1].payload, None);
    let file = recording(&records);
    assert!(!contains(&file, EMAIL.as_bytes()), "the email was recorded");
    let mut out = Vec::new();
    dump(&mut file.as_slice(), &mut out).expect("dumps");
    assert_eq!(
        String::from_utf8(out).expect("utf-8"),
        format!(
            "    0 Control shell -> brain  AccountOp(SignIn)\n    1 Control brain -> shell  Account ({} bytes)\n2 frames\n",
            wire_len(&view, Pipe::Control)
        )
    );
}

#[test]
fn connection_ops_warm_and_the_connections_view_keep_their_fields() {
    let records = [
        record(
            Direction::ToBrain,
            &ControlToBrain::Connection(ConnectionOp::CheckAnthropicKey),
        ),
        record(
            Direction::ToBrain,
            &ControlToBrain::Warm {
                thread: ThreadId(3),
            },
        ),
        record(
            Direction::ToShell,
            &ControlToShell::Connections(ConnectionsView {
                anthropic: KeyView::Unconfirmed,
                claude_code: ClaudeCodeView::SignedOut {
                    version: ClaudeVersion {
                        major: 2,
                        minor: 1,
                        patch: 306,
                    },
                },
            }),
        ),
    ];
    let mut out = Vec::new();
    dump(&mut recording(&records).as_slice(), &mut out).expect("dumps");
    assert_eq!(
        String::from_utf8(out).expect("utf-8"),
        "    0 Control shell -> brain  Connection(CheckAnthropicKey)\n    1 Control shell -> brain  Warm { thread: ThreadId(3) }\n    2 Control brain -> shell  Connections(ConnectionsView { anthropic: Unconfirmed, claude_code: SignedOut { version: ClaudeVersion { major: 2, minor: 1, patch: 306 } } })\n3 frames\n"
    );
}

#[test]
fn a_forged_payload_for_an_unlisted_op_still_prints_redacted() {
    let payload = encode(&view(), Pipe::Control).expect("encodes")[HEADER_LEN..].to_vec();
    let forged = Record {
        pipe: Pipe::Control,
        direction: Direction::ToShell,
        op: "Settings".to_owned(),
        len: u32::try_from(payload.len()).expect("small"),
        payload: Some(payload),
    };
    let described = forged.describe();
    assert!(!described.contains(NAME), "{described}");
    assert!(described.starts_with("Settings ("), "{described}");
}

#[test]
fn headers_must_be_this_version() {
    assert!(read_header(&mut &MAGIC[..]).is_ok());
    assert!(matches!(
        read_header(&mut &MAGIC[..3]),
        Err(FrameError::Truncated)
    ));
    for bad in [*b"KTAP\x01", *b"XXXX\x02", [0, 0, 0, 0, 0]] {
        assert!(
            matches!(read_header(&mut &bad[..]), Err(FrameError::Malformed(_))),
            "{bad:?}"
        );
    }
    let mut out = Vec::new();
    assert!(dump(&mut &b"KTAP\x01"[..], &mut out).is_err());
}

#[test]
fn cut_and_corrupt_records_are_errors() {
    let file = recording(&[record(Direction::ToBrain, &ControlToBrain::Pong)]);
    let body = &file[MAGIC.len()..];
    assert!(read(&mut &body[..0]).expect("clean end").is_none());
    for cut in 1..body.len() {
        assert!(
            matches!(read(&mut &body[..cut]), Err(FrameError::Truncated)),
            "cut at {cut}"
        );
    }
    let flag_at = body.len() - 1 - 1;
    let corrupt = |at: usize, value: u8| {
        let mut bytes = body.to_vec();
        bytes[at] = value;
        read(&mut bytes.as_slice()).map(|_| ())
    };
    assert!(
        matches!(corrupt(0, 7), Err(FrameError::Malformed(_))),
        "pipe"
    );
    assert!(
        matches!(corrupt(1, 7), Err(FrameError::Malformed(_))),
        "direction"
    );
    assert!(
        matches!(corrupt(2, 0), Err(FrameError::Malformed(_))),
        "empty op"
    );
    assert!(
        matches!(corrupt(3, b' '), Err(FrameError::Malformed(_))),
        "op text"
    );
    assert!(
        matches!(corrupt(flag_at, 2), Err(FrameError::Malformed(_))),
        "flag"
    );
    let op_end = 3 + usize::from(body[2]);
    let mut over = body[..op_end].to_vec();
    over.extend(u32::MAX.to_le_bytes());
    over.push(0);
    assert!(matches!(
        read(&mut over.as_slice()),
        Err(FrameError::TooLarge { .. })
    ));
}

#[test]
fn a_listed_payload_must_match_its_length() {
    let mut pong = record(Direction::ToBrain, &ControlToBrain::Pong);
    pong.len += 1;
    assert!(write(&mut Vec::new(), &pong).is_err());
}
