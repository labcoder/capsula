use std::os::windows::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use super::{CREATE_NO_WINDOW, Job, command_role, helper, helper_role};

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
        assert_eq!(
            helper_role(child.0.id()).ok(),
            expected,
            "child launched with {argument}"
        );
        child.0.kill().expect("end fixture child");
        child.0.wait().expect("confirm child exited");
        assert!(
            helper_role(child.0.id()).is_err(),
            "an exited child is never identified"
        );
    }
    assert!(helper_role(0).is_err(), "an unreadable process is refused");
}

#[test]
fn a_native_helper_reads_as_its_canonical_image_role_and_job_and_is_gone_once_it_exits() {
    let root = std::env::var_os("SystemRoot").expect("Windows folder");
    let image = Path::new(&root).join("System32/findstr.exe");
    let mut child = WaitingChild(
        Command::new(&image)
            .arg("--screen")
            .creation_flags(CREATE_NO_WINDOW.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("harmless child waiting on stdin"),
    );
    let job = Job::kill_on_close().expect("a job");
    let outside = helper(child.0.id(), job.held())
        .expect("a live child is read")
        .expect("a live child is there");
    assert!(!outside.in_job, "a child outside the job reads as outside");
    job.assign(&child.0).expect("the child joins the job");
    let read = helper(child.0.id(), job.held())
        .expect("a live child is read")
        .expect("a live child is there");
    assert!(read.in_job, "a child in the job reads as in it");
    assert_eq!(
        read.image,
        std::fs::canonicalize(&image).expect("the image's canonical path")
    );
    assert_eq!(read.role, Some("screen-worker"));
    child.0.kill().expect("end fixture child");
    child.0.wait().expect("confirm child exited");
    assert!(
        helper(child.0.id(), job.held())
            .expect("an exited child reads")
            .is_none(),
        "an exited child is gone, never identified"
    );
}
