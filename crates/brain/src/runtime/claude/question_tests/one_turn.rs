//! The desk's one-turn picture check, driven through the real supervisor and the fake child in a
//! child test process: only that process's environment sets the flag and the one-question budget,
//! so no test changes this process's environment.

use std::io::{self, Read as _};
use std::process::{Command, Stdio};

use kumochi_protocol::constants::{CLAUDE_ONE_TURN_ENV, CLAUDE_QUESTIONS_ENV};

use super::*;

/// Asks one Kumo build that offers both tools and checks what the supervisor gave the fake
/// child, then asks again and checks the second seed is refused. `argv` (after the launch's own
/// config path, which the base's space puts inside the recorded line), `seeded` and `binds` are
/// what the first launch must show.
async fn ask_twice(script: &Script, argv: &str, seeded: &[&str], binds: usize) {
    let mut rig = Rig::new("one turn", script).await;
    let mut request = choosing("haiku", Effort::Low, ConnectionKind::ClaudeCode);
    request.request.tools = crate::context::tools(Origin::Kumo);
    let question = rig.claude.question(&request).expect("admitted");
    let (id, _) = rig.last();
    let mut played = play_with(&question, request.clone());
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    answered(&rig.drive(played.events()).await);
    assert_eq!(
        rig.reported(id).await,
        [launch::Outcome::Clean],
        "both audits"
    );
    let config = rig
        .base
        .owned()
        .join(ENGINE_DIR)
        .join(id.hex())
        .join("work")
        .join("mcp.json");
    let expected = format!(
        "-p --restricted --strict-mcp-config --mcp-config {} {argv}",
        config.display()
    );
    assert!(rig.argv(1) == expected, "the first launch's argv");
    assert_tool_seed(&rig.seed(1), seeded);
    assert_eq!(rig.binds.len(), binds, "binds before the seed");

    let question = rig.claude.question(&request).expect("admitted");
    let mut played = play_with(&question, request);
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    assert_eq!(
        rig.drive(played.events()).await,
        [claude_failed(ErrorKey::ClaudeCodeBudget)]
    );
    assert_eq!(
        rig.stdin()
            .iter()
            .filter(|line| line.ends_with(" user"))
            .count(),
        1,
        "one seed in all"
    );
    rig.close().await;
}

/// With the flag: `--max-turns 1`, `dontAsk` and no allow list, an empty offer that both
/// audits and the seed expect, no bind, and one question.
#[tokio::test]
#[ignore = "run in a child process by the_one_turn_flag_reaches_only_its_own_child_process"]
async fn with_the_one_turn_flag() {
    ask_twice(
        &offering_nothing(),
        "--disable-slash-commands --tools  \
         --no-session-persistence --permission-mode dontAsk --input-format stream-json \
         --output-format stream-json --verbose --include-partial-messages --max-turns 1 \
         --exclude-dynamic-system-prompt-sections --model haiku --effort low",
        &[],
        0,
    )
    .await;
}

/// Without it, the same build is a stock question: both tools, `--max-turns 8`, one bind, and
/// still one question under the budget.
#[tokio::test]
#[ignore = "run in a child process by the_one_turn_flag_reaches_only_its_own_child_process"]
async fn without_the_one_turn_flag() {
    ask_twice(
        &synthetic_signed_in()
            .pre_tools(BOTH_TOOLS)
            .post_tools(BOTH_TOOLS),
        &format!(
            "--disable-slash-commands --tools  \
             --no-session-persistence --allowedTools mcp__{MCP_SERVER_NAME}__draft_issue \
             mcp__{MCP_SERVER_NAME}__draw --input-format stream-json --output-format stream-json \
             --verbose --include-partial-messages --max-turns 8 \
             --exclude-dynamic-system-prompt-sections --model haiku --effort low"
        ),
        BOTH_TOOLS,
        1,
    )
    .await;
}

/// Runs each case above in its own test process, with the budget at one question and the flag
/// set only for the one-turn case, and checks it ran and passed.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "each child test process starts under platform::spawn_lock"
)]
fn the_one_turn_flag_reaches_only_its_own_child_process() {
    let exe = std::env::current_exe().expect("the test binary");
    for (case, flag) in [
        ("with_the_one_turn_flag", true),
        ("without_the_one_turn_flag", false),
    ] {
        let mut child = Command::new(&exe);
        child
            .args([
                "--exact",
                &format!("runtime::claude::question_tests::one_turn::{case}"),
                "--ignored",
                "--test-threads=1",
            ])
            .env(CLAUDE_QUESTIONS_ENV, "1")
            .env_remove(CLAUDE_ONE_TURN_ENV);
        if flag {
            child.env(CLAUDE_ONE_TURN_ENV, "1");
        }
        let mut running = {
            let _spawn = crate::platform::spawn_lock();
            child
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
        }
        .expect("the child test process starts");
        // Read to the end, then wait whatever the read gave: the pipe is dropped before the wait,
        // so a child still writing sees it closed and ends, and nothing panics while it runs.
        let read = running.stdout.take().map_or_else(
            || Err(io::Error::other("no stdout pipe")),
            |mut stdout| {
                let mut bytes = Vec::new();
                stdout.read_to_end(&mut bytes).map(|_| bytes)
            },
        );
        let status = running.wait();
        let stdout = read.expect("the child's stdout");
        let status = status.expect("the child ended");
        assert!(
            status.success()
                && String::from_utf8_lossy(&stdout).contains("test result: ok. 1 passed"),
            "{case}"
        );
    }
}
