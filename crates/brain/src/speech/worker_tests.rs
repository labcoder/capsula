use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStringExt as _;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use super::*;
use crate::platform::sherpa::tests::{Failure, failed_load};

struct Input {
    events: mpsc::Sender<&'static str>,
    leave: mpsc::Receiver<()>,
}

impl Read for Input {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        self.events.send("read").expect("observer");
        self.leave
            .recv_timeout(Duration::from_secs(5))
            .expect("release read");
        Ok(0)
    }
}

struct Output {
    events: mpsc::Sender<&'static str>,
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut written = self.bytes.lock().expect("output");
        if written.is_empty() {
            self.events.send("reply").expect("observer");
        }
        written.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn failed_load_releases_before_receipt(failure: Failure, expected: &[&str]) {
    let (events, observed) = mpsc::channel();
    let (release, leave) = mpsc::channel();
    let (read, next) = mpsc::channel();
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let output = Mutex::new(Output {
        events: events.clone(),
        bytes: Arc::clone(&bytes),
    });
    let mut input = Input {
        events: events.clone(),
        leave: next,
    };
    let models = crate::platform::sherpa::tests::model_fixture();
    let worker = std::thread::spawn(move || {
        loaded_with(&mut input, &output, 17, models.dir(), || {
            Ok(failed_load(failure, events, leave))
        })
    });
    let mut trace = Vec::new();
    loop {
        let event = observed
            .recv_timeout(Duration::from_secs(5))
            .expect("worker progress");
        trace.push(event);
        if event == "resources retiring" || event == "read" {
            break;
        }
    }
    let held_bytes = bytes.lock().expect("output").len();
    release.send(()).expect("release destructor");
    read.send(()).expect("release next read");
    assert!(worker.join().expect("worker").expect("framing").is_none());
    trace.extend(observed.try_iter());
    assert_eq!(held_bytes, 0, "no release receipt while resources are held");
    assert_eq!(trace, expected);
    let written = bytes.lock().expect("output");
    assert_eq!(
        crate::supervise::read_request::<SpeechReply>(&mut written.as_slice(), Speech::MAX_REPLY)
            .expect("framed reply"),
        Some((17, SpeechReply::Failed(NativeError::Create)))
    );
}

#[test]
fn failed_warmup_destroys_the_recognizer_and_resources_before_failed_and_next_read() {
    for (failure, expected) in [
        (
            Failure::Warmup,
            &[
                "recognizer released",
                "resources retiring",
                "resources released",
                "reply",
                "read",
            ][..],
        ),
        (
            Failure::Recognizer,
            &["resources retiring", "resources released", "reply", "read"][..],
        ),
    ] {
        failed_load_releases_before_receipt(failure, expected);
    }
}

const WAIT: Duration = Duration::from_secs(5);

struct ShortOutput {
    bytes: Arc<Mutex<Vec<u8>>>,
    flushed: mpsc::Sender<u64>,
    pause: Arc<Mutex<Option<mpsc::Receiver<()>>>>,
    writing: mpsc::Sender<()>,
}

impl Write for ShortOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = bytes.len().min(3);
        self.bytes
            .lock()
            .expect("bytes")
            .extend_from_slice(&bytes[..count]);
        let pause = self.pause.lock().expect("write gate").take();
        if let Some(pause) = pause {
            self.writing.send(()).expect("partial frame observer");
            pause.recv_timeout(WAIT).expect("release partial frame");
        }
        std::thread::yield_now();
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        let bytes = self.bytes.lock().expect("bytes");
        let mut input = bytes.as_slice();
        let mut last = None;
        while let Some((id, _)) =
            read_request::<SpeechReply>(&mut input, Speech::MAX_REPLY).expect("whole frame")
        {
            last = Some(id);
        }
        self.flushed
            .send(last.expect("a reply was written"))
            .expect("flush observer");
        Ok(())
    }
}

#[test]
fn native_slots_reuse_the_finished_call_and_frame_whole_replies_before_draining() {
    for ending in 0..4 {
        let (first_release, first_gate) = mpsc::channel();
        let (second_release, second_gate) = mpsc::channel();
        let (third_release, third_gate) = mpsc::channel();
        let (first_reply, first_reply_gate) = mpsc::channel();
        let (third_reply, third_reply_gate) = mpsc::channel();
        let (started, observed) = mpsc::channel();
        let (events, releases) = mpsc::channel();
        let (release_resources, resource_gate) = mpsc::channel();
        let (flushed, acknowledgments) = mpsc::channel();
        let (writing, writes) = mpsc::channel();
        let (resume, write_gate) = mpsc::channel();
        let pause = Arc::new(Mutex::new(None));
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let output = Mutex::new(ShortOutput {
            bytes: Arc::clone(&bytes),
            flushed,
            pause: Arc::clone(&pause),
            writing,
        });
        let input = native_requests(ending);
        let models = crate::platform::sherpa::tests::model_fixture();
        let worker = std::thread::spawn(move || {
            let gates = [(2, first_gate), (3, second_gate), (4, third_gate)].into();
            let reply_gates = [(2, first_reply_gate), (4, third_reply_gate)].into();
            loaded_with(&mut input.as_slice(), &output, 17, models.dir(), || {
                Ok(crate::platform::sherpa::tests::parallel_load(
                    gates,
                    reply_gates,
                    started,
                    events,
                    resource_gate,
                ))
            })
        });
        assert_eq!(observed.recv_timeout(WAIT).expect("warmup"), 16_000);
        assert_eq!(acknowledgments.recv_timeout(WAIT).expect("loaded"), 17);
        let mut active = [
            observed.recv_timeout(WAIT).expect("decode"),
            observed.recv_timeout(WAIT).expect("decode"),
        ];
        active.sort_unstable();
        assert_eq!(active, [2, 3]);
        assert_eq!(observed.try_recv(), Err(mpsc::TryRecvError::Empty));
        second_release.send(()).expect("second release");
        assert_eq!(
            acknowledgments.recv_timeout(WAIT).expect("second reply"),
            22
        );
        assert_eq!(observed.recv_timeout(WAIT).expect("reused slot"), 4);
        assert_eq!(releases.try_recv(), Err(mpsc::TryRecvError::Empty));
        first_release.send(()).expect("first release");
        assert_eq!(observed.recv_timeout(WAIT).expect("first reply ready"), 2);
        *pause.lock().expect("write gate") = Some(write_gate);
        first_reply.send(()).expect("first reply release");
        writes.recv_timeout(WAIT).expect("partial first frame");
        third_release.send(()).expect("third release");
        assert_eq!(observed.recv_timeout(WAIT).expect("third reply ready"), 4);
        assert_eq!(acknowledgments.try_recv(), Err(mpsc::TryRecvError::Empty));
        assert_eq!(releases.try_recv(), Err(mpsc::TryRecvError::Empty));
        third_reply.send(()).expect("third reply release");
        resume.send(()).expect("release partial first frame");
        let mut completed = [
            acknowledgments.recv_timeout(WAIT).expect("competing reply"),
            acknowledgments.recv_timeout(WAIT).expect("competing reply"),
        ];
        completed.sort_unstable();
        assert_eq!(completed, [21, 23]);
        assert_eq!(
            releases.recv_timeout(WAIT).expect("recognizer"),
            "recognizer released"
        );
        assert_eq!(
            releases.recv_timeout(WAIT).expect("resources"),
            "resources retiring"
        );
        assert_eq!(acknowledgments.try_recv(), Err(mpsc::TryRecvError::Empty));
        release_resources.send(()).expect("release resources");
        assert_eq!(
            releases.recv_timeout(WAIT).expect("released"),
            "resources released"
        );
        let result = worker.join().expect("worker");
        assert_native_end(ending, result, &bytes.lock().expect("bytes"));
    }
}

fn assert_native_end(ending: usize, result: Result<Next, FrameError>, bytes: &[u8]) {
    match ending {
        0 | 2 => assert!(matches!(result, Ok(None))),
        1 => {
            let Some((30, SpeechRequest::Load { natives, models })) = result.expect("next load")
            else {
                panic!("next load");
            };
            assert_eq!(natives, Path::new("natives"));
            assert_eq!(models, Path::new("new-models"));
        }
        3 => assert!(matches!(result, Err(FrameError::Truncated))),
        _ => unreachable!(),
    }
    let mut input = bytes;
    let mut replies = Vec::new();
    while let Some(reply) =
        read_request::<SpeechReply>(&mut input, Speech::MAX_REPLY).expect("framed reply")
    {
        replies.push(reply);
    }
    let mut expected = vec![
        (17, SpeechReply::Loaded),
        (22, SpeechReply::Text("utterance 3".into())),
        (21, SpeechReply::Text("utterance 2".into())),
        (23, SpeechReply::Text("utterance 4".into())),
    ];
    if ending == 0 {
        expected.push((30, SpeechReply::Unloaded));
    }
    assert_eq!(replies.len(), expected.len());
    replies[2..4].sort_unstable_by_key(|(id, _)| *id);
    assert_eq!(replies, expected);
}

fn native_requests(ending: usize) -> Vec<u8> {
    let mut input = Vec::new();
    for (id, samples) in [(21, 2), (22, 3), (23, 4)] {
        write_message(
            &mut input,
            id,
            &SpeechRequest::Decode {
                rate: 16_000,
                pcm: vec![0; samples * 2],
            },
            Speech::MAX_REQUEST,
        )
        .expect("request");
    }
    match ending {
        0 => write_message(&mut input, 30, &SpeechRequest::Unload, Speech::MAX_REQUEST)
            .expect("unload"),
        1 => write_message(
            &mut input,
            30,
            &SpeechRequest::Load {
                natives: "natives".into(),
                models: "new-models".into(),
            },
            Speech::MAX_REQUEST,
        )
        .expect("load"),
        2 => {}
        3 => input.push(0xff),
        _ => unreachable!(),
    }
    input
}

#[test]
#[expect(clippy::disallowed_methods, reason = "spawns while holding spawn_lock")]
fn a_decode_child_fail_stops_without_another_input_request() {
    const KEY: &str = "KUMOCHI_TEST_DECODE_FAILSTOP";
    const STARTED: &str = "speech decode fixture started";
    const WINDOWS_FAST_FAIL: i32 = -1_073_740_791;
    if let Ok(mode) = std::env::var(KEY) {
        crate::platform::quiet_failures();
        std::thread::scope(|scope| {
            let (release, wait) = mpsc::channel();
            let mut slots = Slots::new(scope);
            let completion_failure = mode == "completion";
            slots
                .start(move || {
                    wait.recv_timeout(WAIT).expect("release child");
                    match mode.as_str() {
                        "output" => send(
                            &Mutex::new(BrokenOutput),
                            17,
                            &SpeechReply::Text("fixture".into()),
                        ),
                        "panic" => panic!("decode fixture panic"),
                        "completion" => Ok(()),
                        _ => unreachable!(),
                    }
                })
                .expect("decode thread");
            if completion_failure {
                drop(slots.freed);
            }
            writeln!(io::stdout(), "{STARTED}").expect("write fixture marker");
            io::stdout().flush().expect("flush fixture marker");
            release.send(()).expect("release child");
            loop {
                std::thread::park();
            }
        });
    }
    for mode in ["output", "panic", "completion"] {
        let mut command =
            std::process::Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "speech::worker::tests::a_decode_child_fail_stops_without_another_input_request",
                "--nocapture",
            ])
            .env(KEY, mode)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = {
            let _spawning = crate::platform::spawn_lock();
            command.spawn().expect("child test process")
        };
        let until = std::time::Instant::now() + WAIT;
        loop {
            if let Some(status) = child.try_wait().expect("child status") {
                assert_eq!(status.code(), Some(WINDOWS_FAST_FAIL), "{mode}");
                let mut output = String::new();
                child
                    .stdout
                    .take()
                    .expect("fixture stdout")
                    .read_to_string(&mut output)
                    .expect("read fixture marker");
                assert!(output.contains(STARTED), "{mode}: fixture did not start");
                break;
            }
            if std::time::Instant::now() >= until {
                child.kill().expect("stop hung fixture");
                child.wait().expect("join hung fixture");
                panic!("child failure did not stop a blocked owner");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

struct BrokenOutput;

impl Write for BrokenOutput {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::BrokenPipe.into())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn load_refusal(models: &Path) -> SpeechReply {
    let mut queued = Vec::new();
    write_message(&mut queued, 42, &SpeechRequest::Unload, Speech::MAX_REQUEST)
        .expect("queued unload");
    let mut input = queued.as_slice();
    let output = Mutex::new(Vec::new());
    let next = loaded(&mut input, &output, 41, Path::new("\0"), models, || {
        Err(NativeError::Unsafe)
    })
    .expect("load refusal and continuation");
    assert!(
        matches!(next, Some((42, SpeechRequest::Unload))),
        "the next request survives the failed load"
    );
    assert!(input.is_empty(), "the queued request was consumed once");
    let output = output.into_inner().expect("fixture output");
    let mut output = output.as_slice();
    let (id, reply) = read_request::<SpeechReply>(&mut output, Speech::MAX_REPLY)
        .expect("framed refusal")
        .expect("one reply");
    assert_eq!(id, 41, "the refusal answers the load");
    assert!(output.is_empty(), "exactly one reply");
    reply
}

#[test]
fn an_unrepresentable_model_path_is_refused_without_native_storage() {
    let non_utf8 = OsString::from_wide(&[0xd800]);
    for models in [
        Path::new("\0"),
        Path::new("models\0private"),
        Path::new(&non_utf8),
    ] {
        assert_eq!(
            load_refusal(models),
            SpeechReply::Failed(NativeError::Input)
        );
    }

    for models in ["", ".", "models", "modèles", "C:\\models\\音声"] {
        assert_eq!(
            load_refusal(Path::new(models)),
            SpeechReply::Failed(NativeError::Unsafe)
        );
    }
}
