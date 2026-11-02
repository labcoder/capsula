//! The messages on each pipe, as closed enums. The shell's first frame on every pipe is a
//! [`Hello`], and the brain decodes nothing else until the capability in it checks out.
//!
//! postcard encodes a variant by its position, so a new variant goes at the end of its enum and
//! the handshake's bytes stay readable by other builds.

use serde::{Deserialize, Serialize};

use crate::Secret;

/// Length of the instance capability, in bytes.
pub const CAPABILITY_LEN: usize = 32;

/// The random secret the brain rotates on every start and stores in Credential Manager. A shell
/// proves it runs as the same user by reading it back and sending it in [`Hello`].
pub type Capability = Secret<[u8; CAPABILITY_LEN]>;

/// One run of the brain, random per start. A reconnect to a new incarnation discards everything
/// in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Incarnation(pub u64);

/// One thread of conversation, as the store numbers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ThreadId(pub u64);

/// One user turn, and the epoch every run-scoped message carries. Ids only grow, even across
/// restarts, so a stale message can never pass for a new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RunId(pub u64);

/// The shell's first frame on each pipe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The shell's [`crate::BUILD_ID`].
    pub build_id: String,
    /// The capability the shell read from Credential Manager.
    pub capability: Capability,
}

impl Hello {
    /// A copy with the capability zeroed, for the pipe tap.
    #[must_use]
    pub fn redacted(&self) -> Self {
        Self {
            build_id: self.build_id.clone(),
            capability: Secret::new([0; CAPABILITY_LEN]),
        }
    }
}

/// The most steps one run may draw (architecture v1 §5.2).
pub const MAX_STEPS: usize = 64;

/// The longest placeholder step label, in bytes [guess].
pub const MAX_STEP_LABEL: usize = 256;

/// Bytes of data-pipe frames the brain may send before the shell grants more with
/// [`DataToBrain::Credit`] [guess]. It holds the largest frame the data pipe allows, so no frame
/// is too big to send, and several of the 64 KiB text frames runs stream, so a reading shell
/// rarely waits on a grant.
pub const DATA_WINDOW: u32 = 512 * 1024;

/// One drawing step. shortcut: a labelled placeholder so runs, recovery and Esc can be built
/// and tested; slice 4 replaces it with the `scene` v1 step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// What the step shows, at most [`MAX_STEP_LABEL`] bytes.
    pub label: String,
}

/// Where a run is, as the shell shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// Accepted; nothing has streamed yet.
    Thinking,
    /// Text or steps are arriving.
    Answering,
    /// Finished.
    Done,
    /// Stopped before it finished.
    Interrupted,
    /// Ended by an error.
    Failed,
}

/// Shell to brain, on the control pipe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlToBrain {
    /// The handshake; first and only once.
    Hello(Hello),
    /// The user chose Quit: the brain ends its helpers, deletes the capability and exits, and
    /// doesn't relaunch the shell.
    Quitting,
    /// The first Esc, after the shell has already stopped locally. `revealed` lists the steps
    /// whose entrance had finished, so the thread records what stayed; the brain keeps only
    /// indices of steps the run produced, and the control cap bounds the list.
    Stop {
        /// The run the shell stopped.
        run: RunId,
        /// Indices of the steps that stayed on screen.
        revealed: Vec<u32>,
    },
    /// The answer to [`ControlToShell::Ping`].
    Pong,
}

/// Brain to shell, on the control pipe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlToShell {
    /// The handshake succeeded.
    Welcome {
        /// This brain's incarnation.
        incarnation: Incarnation,
    },
    /// The shell is from another build. The brain exits after sending this, so the shell can
    /// start the brain from its own folder.
    BuildMismatch {
        /// The brain's [`crate::BUILD_ID`].
        brain_build: String,
    },
    /// A run's phase changed.
    RunState {
        /// The run.
        run: RunId,
        /// Its thread.
        thread: ThreadId,
        /// Its new phase.
        phase: Phase,
    },
    /// The watchdog, sent only while a run is active; the shell answers [`ControlToBrain::Pong`].
    Ping,
}

/// Shell to brain, on the data pipe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataToBrain {
    /// The handshake, repeated so the data pipe is authenticated on its own.
    Hello(Hello),
    /// Typed chat: a new turn in `thread`, or in a new thread when it's `None`.
    TextSubmit {
        /// The thread to continue.
        thread: Option<ThreadId>,
        /// What the user typed.
        text: String,
    },
    /// The shell has taken `bytes` more of the brain's data frames, header included, and the
    /// brain may send that many more.
    Credit {
        /// How many bytes.
        bytes: u32,
    },
}

/// Brain to shell, on the data pipe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataToShell {
    /// More of the assistant's answer.
    AssistantText {
        /// The run it belongs to.
        run: RunId,
        /// The new text.
        delta: String,
    },
    /// One step, sent only once the brain has committed it.
    StepCommit {
        /// The run it belongs to.
        run: RunId,
        /// Its place in the run's drawing, from 0.
        index: u32,
        /// The step.
        step: Step,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        CAPABILITY_LEN, ControlToBrain, ControlToShell, DataToBrain, Hello, Incarnation, Secret,
    };

    /// The bytes another build must be able to read: the handshake and its answers. A new
    /// variant goes at the end of its enum, or these tags move.
    #[test]
    fn handshake_frames_keep_their_bytes_across_builds() {
        let hello = Hello {
            build_id: "b".to_owned(),
            capability: Secret::new([7; CAPABILITY_LEN]),
        };
        let mut hello_bytes = vec![0, 1, b'b'];
        hello_bytes.extend([7; CAPABILITY_LEN]);
        let encode = |bytes: postcard::Result<Vec<u8>>| bytes.expect("encodes");
        assert_eq!(
            encode(postcard::to_allocvec(&ControlToBrain::Hello(hello.clone()))),
            hello_bytes
        );
        assert_eq!(
            encode(postcard::to_allocvec(&DataToBrain::Hello(hello))),
            hello_bytes
        );
        assert_eq!(
            encode(postcard::to_allocvec(&ControlToBrain::Quitting)),
            [1]
        );
        assert_eq!(
            encode(postcard::to_allocvec(&ControlToShell::Welcome {
                incarnation: Incarnation(5)
            })),
            [0, 5]
        );
        assert_eq!(
            encode(postcard::to_allocvec(&ControlToShell::BuildMismatch {
                brain_build: "b".to_owned()
            })),
            [1, 1, b'b']
        );
    }
}
