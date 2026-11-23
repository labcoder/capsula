use std::collections::VecDeque;
use std::io::{self, Read};
use std::time::Duration;

use super::{Failure, VERSION_CAP, drain, output};

/// A pipe that gives each step in turn, then its end.
struct Script(VecDeque<io::Result<Vec<u8>>>);

impl Script {
    fn new(steps: impl IntoIterator<Item = io::Result<Vec<u8>>>) -> Self {
        Self(steps.into_iter().collect())
    }
}

impl Read for Script {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0.pop_front() {
            None => Ok(0),
            Some(Err(error)) => Err(error),
            Some(Ok(bytes)) => {
                buf[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
        }
    }
}

const ANSWER: &[u8] = b"2.1.306 (Claude Code)\n";

fn drained(pipe: Script) -> Option<super::Drained> {
    drain(pipe, VERSION_CAP)
        .recv_timeout(Duration::from_secs(5))
        .ok()
}

fn whole() -> Script {
    Script::new([Ok(ANSWER.to_vec())])
}

fn broken() -> Script {
    Script::new([Ok(ANSWER.to_vec()), Err(io::ErrorKind::Other.into())])
}

#[test]
fn a_pipe_that_fails_after_a_whole_answer_fails_the_call() {
    assert_eq!(
        output(drained(broken()), drained(Script::new([]))),
        Err(Failure::Pipe),
        "stdout"
    );
    assert_eq!(
        output(drained(whole()), drained(broken())),
        Err(Failure::Pipe),
        "stderr"
    );
}

#[test]
fn an_interrupted_read_is_retried_to_the_end() {
    let interrupted = Script::new([
        Ok(ANSWER[..4].to_vec()),
        Err(io::ErrorKind::Interrupted.into()),
        Ok(ANSWER[4..].to_vec()),
    ]);
    assert_eq!(
        output(drained(interrupted), drained(Script::new([]))),
        Ok(ANSWER.to_vec())
    );
}
