use std::os::windows::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use super::{CREATE_NO_WINDOW, command_role, helper_role, process_command_line};

#[derive(Debug)]
struct WaitingChild(Child);

impl Drop for WaitingChild {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _killed = self.0.kill();
        }
        let _reaped = self.0.wait();
    }
}

#[test]
fn role_arguments_use_windows_quoting_and_reject_extra_or_unterminated_arguments() {
    for (text, expected) in [
        (
            r#""C:\Program Files\fixture.exe" --screen"#,
            Some("screen-worker"),
        ),
        (
            r#""C:\Program Files\fixture.exe" --speech"#,
            Some("speech-worker"),
        ),
        ("fixture.exe --screen --speech", None),
        ("fixture.exe --screen-extra", None),
        ("fixture.exe", None),
    ] {
        let line: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        assert_eq!(command_role(&line).ok(), expected, "exact argument grammar");
    }
    assert!(command_role(&[]).is_err(), "a missing buffer is refused");
    assert!(
        command_role(&[45, 45, 115]).is_err(),
        "a missing NUL is refused"
    );
}

#[test]
fn native_command_lines_identify_exact_roles_and_refuse_unknown_or_exited_children() {
    let root = std::env::var_os("SystemRoot").expect("Windows folder");
    for (argument, expected) in [
        ("--screen", Some("screen-worker")),
        ("--speech", Some("speech-worker")),
        ("--unknown", None),
        ("--screen-extra", None),
        ("--Screen", None),
    ] {
        let mut child = WaitingChild(
            Command::new(Path::new(&root).join("System32/findstr.exe"))
                .arg(argument)
                .creation_flags(CREATE_NO_WINDOW.0)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("harmless child waiting on stdin"),
        );
        let line = process_command_line(child.0.id()).expect("native child command line");
        assert_eq!(
            line.last(),
            Some(&0),
            "reader supplies the parser's terminating NUL"
        );
        let text = String::from_utf16(&line[..line.len() - 1]).expect("UTF-16 command line");
        assert!(
            text.ends_with(argument),
            "the known trailing argument was read"
        );
        assert_eq!(
            command_role(&line).ok(),
            expected,
            "exact native role parse"
        );
        assert_eq!(
            helper_role(child.0.id()).ok(),
            expected,
            "actual helper role entry point"
        );
        child.0.kill().expect("end fixture child");
        child.0.wait().expect("confirm child exited");
        assert!(
            process_command_line(child.0.id()).is_err(),
            "an exited child is never identified"
        );
    }
    assert!(
        process_command_line(0).is_err(),
        "an unreadable process is refused"
    );
}
