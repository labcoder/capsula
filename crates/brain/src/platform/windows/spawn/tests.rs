//! Spawn tests. Each child is this test binary re-entered at [`child_probe`], which reports its
//! arguments, environment and console handles, then acts on a mode.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use super::*;

const PROBE: &str = "platform::windows::spawn::tests::child_probe";
/// Marks where the probe's own arguments start among the test harness's filters.
const MARK: &str = "spawn-probe";

fn exe() -> PathBuf {
    std::env::current_exe().expect("test executable")
}

fn probe_args(mode: &str, extra: &[OsString]) -> Vec<OsString> {
    let harness = [
        "--exact",
        PROBE,
        "--ignored",
        "--nocapture",
        "--test-threads=1",
        "-q",
        MARK,
    ];
    harness
        .iter()
        .chain(&[mode])
        .map(OsString::from)
        .chain(extra.iter().cloned())
        .collect()
}

fn base_env() -> Vec<(OsString, OsString)> {
    vec![(
        "SystemRoot".into(),
        std::env::var_os("SystemRoot").expect("SystemRoot is set"),
    )]
}

fn start(
    mode: &str,
    extra: &[OsString],
    env: &[(OsString, OsString)],
    stdio: Stdio,
    job: &Job,
) -> io::Result<Spawned> {
    let exe = exe();
    spawn(&Launch {
        exe: &exe,
        args: &probe_args(mode, extra),
        cwd: exe.parent().expect("folder"),
        env,
        stdio,
        jobs: &[job],
    })
}

/// The probe's report lines from a child's whole stdout.
fn report(spawned: &mut Spawned) -> Vec<String> {
    let mut text = String::new();
    spawned
        .stdout
        .take()
        .expect("piped")
        .read_to_string(&mut text)
        .expect("stdout");
    text.lines()
        .filter(|line| line.starts_with("ARG ") || line.starts_with("ENV "))
        .map(str::to_owned)
        .collect()
}

/// Reads `pipe` to its end on a thread; the receiver gets the bytes once it's closed.
fn read_to_end(mut pipe: PipeReader) -> mpsc::Receiver<Vec<u8>> {
    let (done, ended) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if pipe.read_to_end(&mut bytes).is_ok() {
            done.send(bytes).unwrap_or_default();
        }
    });
    ended
}

#[test]
#[ignore = "only runs as the spawn tests' child"]
fn child_probe() {
    let args: Vec<OsString> = std::env::args_os().collect();
    let Some(at) = args.iter().position(|arg| arg == MARK) else {
        return;
    };
    let mode = args[at + 1].to_str().expect("mode").to_owned();
    let own = &args[at + 2..];
    let mut lines = String::new();
    for arg in own {
        writeln!(lines, "ARG {arg:?}").expect("a String takes writes");
    }
    for (name, value) in std::env::vars_os() {
        writeln!(lines, "ENV {name:?}={value:?}").expect("a String takes writes");
    }
    match mode.as_str() {
        "report" => {
            let mut out = io::stdout();
            out.write_all(lines.as_bytes()).expect("stdout");
            out.flush().expect("flush");
        }
        "echo" => {
            let mut input = Vec::new();
            io::stdin().read_to_end(&mut input).expect("stdin");
            let mut out = io::stdout();
            out.write_all(&input).expect("stdout");
            out.flush().expect("flush");
        }
        "console" => {
            let terminal = format!(
                "stdin={} stdout={} stderr={}",
                io::stdin().is_terminal(),
                io::stdout().is_terminal(),
                io::stderr().is_terminal()
            );
            std::fs::write(&own[0], terminal).expect("write the console report");
            loop {
                std::thread::park();
            }
        }
        _ => loop {
            std::thread::park();
        },
    }
}

#[test]
fn arguments_round_trip_through_the_command_line() {
    let job = Job::new().expect("job");
    let args: Vec<OsString> = [
        "",
        "a b",
        "tab\there",
        r#"say "hi""#,
        r"trail\",
        r"two\\",
        r#"back\"quote"#,
        r"C:\dir with space\",
        "plain",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    let mut child = start("report", &args, &base_env(), Stdio::Piped, &job).expect("spawn");
    let args_seen: Vec<String> = report(&mut child)
        .into_iter()
        .filter(|line| line.starts_with("ARG "))
        .collect();
    let expected: Vec<String> = args.iter().map(|arg| format!("ARG {arg:?}")).collect();
    assert_eq!(args_seen, expected);
}

#[test]
fn the_child_sees_only_the_environment_it_was_given() {
    assert!(
        std::env::var_os("PATH").is_some(),
        "the parent has a PATH to leak"
    );
    let job = Job::new().expect("job");
    let mut env = base_env();
    env.push(("Kumochi_Probe".into(), "x = y".into()));
    let mut child = start("report", &[], &env, Stdio::Piped, &job).expect("spawn");
    let env_seen: Vec<String> = report(&mut child)
        .into_iter()
        .filter(|line| line.starts_with("ENV "))
        .collect();
    let root = std::env::var_os("SystemRoot").expect("SystemRoot");
    assert_eq!(
        env_seen,
        vec![
            r#"ENV "Kumochi_Probe"="x = y""#.to_owned(),
            format!("ENV \"SystemRoot\"={root:?}"),
        ]
    );
}

#[test]
fn a_command_line_of_exactly_the_limit_starts_and_one_more_unit_is_refused() {
    let job = Job::new().expect("job");
    let exe = exe();
    let base = command_line(&exe, &probe_args("report", &[]))
        .expect("short line")
        .len();
    // One space joins the filler to the line.
    let fill = MAX_COMMAND_LINE - base - 1;
    let filler = OsString::from("x".repeat(fill));
    let mut child = start(
        "report",
        std::slice::from_ref(&filler),
        &base_env(),
        Stdio::Piped,
        &job,
    )
    .expect("a line of exactly the limit starts");
    assert_eq!(report(&mut child)[0], format!("ARG {filler:?}"));

    // A fresh job, since the first child's console host may linger in the old one.
    let empty = Job::new().expect("job");
    let over = OsString::from("x".repeat(fill + 1));
    let refused = start("report", &[over], &base_env(), Stdio::Piped, &empty)
        .expect_err("one unit more is refused");
    assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        empty.active_processes().expect("count"),
        0,
        "nothing was created"
    );
}

#[test]
fn pipes_carry_stdin_to_the_child_and_its_stdout_back() {
    let job = Job::new().expect("job");
    let mut child = start("echo", &[], &base_env(), Stdio::Piped, &job).expect("spawn");
    let ended = read_to_end(child.stdout.take().expect("stdout"));
    let mut stdin = child.stdin.take().expect("stdin");
    stdin.write_all(b"ping\n").expect("write");
    drop(stdin);
    let out = ended
        .recv_timeout(Duration::from_secs(10))
        .expect("stdout ends");
    let text = String::from_utf8_lossy(&out);
    assert_eq!(
        text.lines().filter(|line| *line == "ping").count(),
        1,
        "{text:?}"
    );
}

/// Runs the ignored test `name` alone in a child of this test binary, so a handle it makes
/// inheritable can't reach a child another test starts at the same time.
fn run_alone(name: &str) {
    let job = Job::new().expect("job");
    let exe = exe();
    let args: Vec<OsString> = ["--exact", name, "--ignored", "--test-threads=1"]
        .map(OsString::from)
        .to_vec();
    let mut child = spawn(&Launch {
        exe: &exe,
        args: &args,
        cwd: exe.parent().expect("folder"),
        env: &base_env(),
        stdio: Stdio::Piped,
        jobs: &[&job],
    })
    .expect("spawn");
    let ended = read_to_end(child.stdout.take().expect("stdout"));
    let out = ended
        .recv_timeout(Duration::from_secs(30))
        .expect("the isolated test ends");
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("test result: ok. 1 passed"), "{text}");
}

#[test]
fn an_inheritable_brain_handle_stays_with_the_brain() {
    run_alone("platform::windows::spawn::tests::isolated_inheritable_handle");
}

#[test]
#[ignore = "runs alone in its own process; see run_alone"]
fn isolated_inheritable_handle() {
    let job = Job::new().expect("job");
    let (reader, writer) = io::pipe().expect("pipe");
    set_inheritable(writer.as_raw_handle()).expect("inheritable");
    let child = start("park", &[], &base_env(), Stdio::Piped, &job).expect("spawn");
    drop(writer);
    // The only writer left would be the child's copy, if it had inherited one.
    let ended = read_to_end(reader);
    assert_eq!(
        ended
            .recv_timeout(Duration::from_secs(10))
            .expect("EOF while the child lives"),
        Vec::<u8>::new()
    );
    assert!(
        !child.process.exits_within(Duration::ZERO).expect("wait"),
        "still running"
    );
    job.terminate().expect("job ends");
}

#[test]
fn stdout_ends_when_the_job_is_terminated() {
    let job = Job::new().expect("job");
    let mut child = start("park", &[], &base_env(), Stdio::Piped, &job).expect("spawn");
    let ended = read_to_end(child.stdout.take().expect("stdout"));
    assert!(
        ended.recv_timeout(Duration::from_millis(300)).is_err(),
        "open while it parks"
    );
    job.terminate().expect("job ends");
    ended
        .recv_timeout(Duration::from_secs(2))
        .expect("EOF after terminate");
}

#[test]
fn a_write_to_a_dead_child_fails_without_blocking() {
    let job = Job::new().expect("job");
    let mut child = start("report", &[], &base_env(), Stdio::Piped, &job).expect("spawn");
    report(&mut child);
    assert!(
        child
            .process
            .exits_within(Duration::from_secs(10))
            .expect("wait"),
        "exited"
    );
    let error = child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"late")
        .expect_err("nobody reads");
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
}

#[test]
fn null_gives_no_pipes() {
    let job = Job::new().expect("job");
    let child = start("report", &[], &base_env(), Stdio::Null, &job).expect("spawn");
    assert!(child.stdin.is_none() && child.stdout.is_none() && child.stderr.is_none());
    assert!(
        child
            .process
            .exits_within(Duration::from_secs(10))
            .expect("wait"),
        "ran to the end"
    );
}

/// Opens a visible console on the desktop running the tests, so it's run by hand, under the
/// measuring lock, while the owner is away.
#[test]
#[ignore = "opens a visible console; run by hand"]
fn console_gives_the_child_its_own_console_and_terminate_ends_it() {
    let dir = crate::scratch::dir("spawn-console");
    let job = Job::new().expect("job");
    let report_path = dir.join("console.txt");
    let child = start(
        "console",
        &[report_path.clone().into()],
        &base_env(),
        Stdio::Console,
        &job,
    )
    .expect("spawn");
    assert!(child.stdin.is_none() && child.stdout.is_none() && child.stderr.is_none());
    let read = wait_for(&report_path);
    assert_eq!(read, "stdin=true stdout=true stderr=true");
    job.terminate().expect("job ends");
    assert!(
        child
            .process
            .exits_within(Duration::from_secs(1))
            .expect("wait")
    );
    let path = dir.path().to_path_buf();
    drop(dir);
    assert!(!path.try_exists().expect("check cleanup"), "clean up");
}

/// Closing the console window ends the child. Needs a person to close it.
#[test]
#[ignore = "opens a visible console and needs a person to close it"]
fn closing_the_console_ends_the_child() {
    let job = Job::new().expect("job");
    let child = start("park", &[], &base_env(), Stdio::Console, &job).expect("spawn");
    assert!(
        child
            .process
            .exits_within(Duration::from_secs(120))
            .expect("wait"),
        "closed by hand"
    );
}

fn wait_for(path: &Path) -> String {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.is_empty()
        {
            return text;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the console child never reported");
}

fn units(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

#[test]
fn quoting_follows_the_command_line_rules() {
    let quoted = |arg: &str| {
        let mut line = Vec::new();
        push_quoted(&mut line, OsStr::new(arg));
        String::from_utf16(&line).expect("utf-16")
    };
    assert_eq!(quoted("plain"), "plain");
    assert_eq!(quoted(""), r#""""#);
    assert_eq!(quoted("a b"), r#""a b""#);
    assert_eq!(quoted("a\tb"), "\"a\tb\"");
    assert_eq!(quoted(r#"say "hi""#), r#""say \"hi\"""#);
    assert_eq!(quoted(r"no\space"), r"no\space");
    assert_eq!(quoted(r"trail\ x\"), r#""trail\ x\\""#);
    assert_eq!(quoted(r#"a\"b"#), r#""a\\\"b""#);
}

#[test]
fn the_executable_is_quoted_and_a_quote_in_it_is_refused() {
    let line = command_line(Path::new(r"C:\a b\c.exe"), &[OsString::from("x")]).expect("line");
    assert_eq!(line, [units(r#""C:\a b\c.exe" x"#), vec![0]].concat());
    let refused = command_line(Path::new(r#"C:\a"b.exe"#), &[]).expect_err("quote");
    assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);
}

fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).into(), (*value).into()))
        .collect()
}

#[test]
fn the_environment_block_sorts_by_ordinal_uppercase() {
    let block = environment_block(&env(&[("_x", "3"), ("b", "2"), ("A", "1")])).expect("block");
    // Uppercase puts `b` (as `B`, 0x42) before `_` (0x5F); a plain sort would put it after.
    assert_eq!(
        block,
        [
            units("A=1"),
            vec![0],
            units("b=2"),
            vec![0],
            units("_x=3"),
            vec![0, 0]
        ]
        .concat()
    );
}

#[test]
fn an_empty_environment_is_two_nuls() {
    assert_eq!(environment_block(&[]).expect("block"), vec![0, 0]);
}

#[test]
fn bad_environment_entries_are_refused() {
    for bad in [
        env(&[("PATH", "a"), ("Path", "b")]),
        env(&[("", "a")]),
        env(&[("A=B", "c")]),
        env(&[("=C:", r"C:\")]),
        env(&[("A\0B", "c")]),
        env(&[("A", "b\0c")]),
        env(&[("Ärger", "1"), ("ärger", "2")]),
    ] {
        let error = environment_block(&bad).expect_err("refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
    }
}

#[test]
fn a_nul_or_relative_path_is_refused_before_any_child_starts() {
    let job = Job::new().expect("job");
    let exe = exe();
    let folder = exe.parent().expect("folder").to_owned();
    let sentinel: OsString = "later-sentinel".into();
    let nul_exe = PathBuf::from(format!("{}\0tail", exe.display()));
    let nul_cwd = PathBuf::from(format!("{}\0tail", folder.display()));
    let cases: [(&Path, Vec<OsString>, &Path); 7] = [
        (&exe, vec!["first\0tail".into(), sentinel.clone()], &folder),
        (&exe, vec!["\0first".into(), sentinel.clone()], &folder),
        (&nul_exe, vec![sentinel], &folder),
        (Path::new("\0"), vec![], &folder),
        (&exe, vec![], &nul_cwd),
        (Path::new("kumochid.exe"), vec![], &folder),
        (&exe, vec![], Path::new("")),
    ];
    for (exe, args, cwd) in &cases {
        let error = spawn(&Launch {
            exe,
            args,
            cwd,
            env: &base_env(),
            stdio: Stdio::Piped,
            jobs: &[&job],
        })
        .expect_err("refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{exe:?} {cwd:?}");
    }
    assert_eq!(
        job.active_processes().expect("count"),
        0,
        "no child started"
    );
}

#[test]
fn launch_and_spawned_debug_show_counts_not_values() {
    let job = Job::new().expect("job");
    let launch = Launch {
        exe: Path::new(r"C:\k.exe"),
        args: &["secret-arg".into()],
        cwd: Path::new(r"C:\"),
        env: &env(&[("NAME", "secret-value")]),
        stdio: Stdio::Null,
        jobs: &[&job],
    };
    let text = format!("{launch:?}");
    assert_eq!(
        text,
        r#"Launch { exe: "C:\\k.exe", args: 1, env: 1, stdio: Null, jobs: 1, .. }"#
    );
}

#[test]
fn a_shut_down_job_refuses_new_processes() {
    let job = Job::new().expect("job");
    let before = start("park", &[], &base_env(), Stdio::Null, &job).expect("a start before");
    job.shut_down().expect("job ends");
    assert!(
        before
            .process
            .exits_within(Duration::from_secs(2))
            .expect("wait"),
        "ended by the shutdown"
    );
    let after = start("park", &[], &base_env(), Stdio::Null, &job).expect_err("refused");
    assert_eq!(after.to_string(), "the job was shut down");
    let piped = Job::spawn_piped(&exe(), MARK, &[&job]).expect_err("refused");
    assert_eq!(piped.to_string(), "the job was shut down");
    assert_eq!(job.active_processes().expect("count"), 0);
}

#[test]
fn terminating_a_helper_that_is_already_exiting_succeeds() {
    let job = Job::new().expect("job");
    let child = start("park", &[], &base_env(), Stdio::Null, &job).expect("spawn");
    child.process.terminate().expect("terminating");
    child
        .process
        .terminate()
        .expect("a second request while it exits");
    assert!(
        child
            .process
            .exits_within(Duration::from_secs(2))
            .expect("wait")
    );
    job.terminate().expect("job ends");
}
