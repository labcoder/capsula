//! The session between the shell and the brain it starts as a child: a Noise handshake per leg
//! keyed by a fresh one-session group secret, the binding of the two legs, and the records that
//! carry today's frames. No I/O: callers move the bytes. See `docs/development/session.md`.

mod pair;
mod record;

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering};

use kumochi_protocol::constants::SESSION_LABEL;
use zeroize::Zeroize;

pub use pair::{
    BoundPair, BrainAwaitsBind, BrainAwaitsDataFinished, BrainAwaitsDataInit, BrainAwaitsFinished,
    BrainAwaitsInit, Outgoing, Records, Session, ShellAwaitsBindAck, ShellAwaitsDataReply,
    ShellAwaitsReply,
};
pub use record::{Delivery, Opener, Sealer};

/// The Noise protocol: no static key on either side; the group secret is the only PSK.
pub const PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";

/// Bytes in the group secret.
pub const SECRET_LEN: usize = 32;

/// Bytes in the big-endian length envelope before every handshake message and record.
pub const ENVELOPE_LEN: usize = 2;

/// The largest record body: Noise's message cap.
pub const MAX_RECORD: usize = 65_535;

/// The most plaintext one application record carries.
pub const MAX_PLAINTEXT: usize = MAX_RECORD - TAG_LEN;

/// Records per direction per leg, confirmation and binding records included. An operational
/// limit, not a claim about the cipher's bounds: the next seal or open past it fails.
pub const RECORD_LIMIT: u64 = 1 << 32;

const PROLOGUE_VERSION: u8 = 1;
const TAG_LEN: usize = 16;
const HASH_LEN: usize = 32;

const _: () = assert!(
    SESSION_LABEL.len() <= 255,
    "the prologue holds the label's length in a u8"
);

/// Which build the two processes are. There is deliberately no gate value: a gate build can't
/// form a prologue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// Installed builds.
    Release,
    /// Development builds.
    Dev,
}

/// Why a session step was refused. Every refusal is terminal for the whole attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// A record or handshake message whose length this phase doesn't allow.
    #[error("a {len}-byte message where this phase allows another length")]
    Length {
        /// The length that was refused.
        len: usize,
    },
    /// The Noise handshake failed: another secret, prologue or leg, or a broken message.
    #[error("the handshake failed")]
    Handshake,
    /// A record failed to authenticate.
    #[error("a record failed to authenticate")]
    Authentication,
    /// The bind record names a data handshake other than this pair's.
    #[error("the bind record names another data leg")]
    Bind,
    /// A frame header declares more than the leg's cap.
    #[error("a frame header is over the leg's cap")]
    FrameTooLarge,
    /// A record carries bytes past the end of its frame.
    #[error("a record crosses the end of its frame")]
    CrossesFrame,
    /// The stream ended inside a record or a frame.
    #[error("the stream ended inside a record or frame")]
    Truncated,
    /// The direction has used its record limit.
    #[error("the record limit is spent")]
    Exhausted,
    /// A seal while the previous record is still being written.
    #[error("a record is still being written")]
    Pending,
    /// The cipher refused a record.
    #[error("the cipher refused a record")]
    Cipher,
    /// The attempt already failed.
    #[error("the session already failed")]
    Retired,
}

/// One pairing attempt, shared by all of its states and record owners: its record limit and the
/// latch that the first failure sets for every owner of both legs.
#[derive(Debug)]
struct Attempt {
    retired: AtomicBool,
    limit: u64,
}

impl Attempt {
    fn check(&self) -> Result<(), SessionError> {
        if self.retired.load(Ordering::SeqCst) {
            return Err(SessionError::Retired);
        }
        Ok(())
    }

    fn fail<T>(&self, error: SessionError) -> Result<T, SessionError> {
        self.retired.store(true, Ordering::SeqCst);
        Err(error)
    }

    /// The length an envelope announces, when it is exactly what the phase allows.
    fn exact(&self, envelope: [u8; ENVELOPE_LEN], allowed: usize) -> Result<usize, SessionError> {
        self.check()?;
        let len = usize::from(u16::from_be_bytes(envelope));
        if len != allowed {
            return self.fail(SessionError::Length { len });
        }
        Ok(len)
    }
}

/// Which leg a prologue is for. The data leg's prologue carries the control handshake hash.
#[derive(Clone, Copy)]
enum Leg<'a> {
    Control,
    Data(&'a [u8; HASH_LEN]),
}

/// `u8 len ‖ SESSION_LABEL ‖ u8 version ‖ u8 channel ‖ u8 leg`, then the control hash on data.
fn prologue(channel: Channel, leg: Leg<'_>) -> Vec<u8> {
    let label = SESSION_LABEL.as_bytes();
    let mut bytes = Vec::with_capacity(4 + label.len() + HASH_LEN);
    bytes.push(u8::try_from(label.len()).expect("the label length is checked at compile time"));
    bytes.extend_from_slice(label);
    let channel = match channel {
        Channel::Release => 0,
        Channel::Dev => 1,
    };
    bytes.extend([PROLOGUE_VERSION, channel]);
    match leg {
        Leg::Control => bytes.push(0),
        Leg::Data(control_hash) => {
            bytes.push(1);
            bytes.extend_from_slice(control_hash);
        }
    }
    bytes
}

/// Application-owned storage that is overwritten with zeros when dropped. It covers only this
/// buffer: copies the Noise library makes are not wiped (docs/development/session.md).
struct Wiped(Box<[u8]>);

impl Wiped {
    fn zeroed(len: usize) -> Self {
        Self(vec![0; len].into_boxed_slice())
    }
}

impl Deref for Wiped {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl DerefMut for Wiped {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }
}

impl Drop for Wiped {
    fn drop(&mut self) {
        self.0.zeroize();
        #[cfg(test)]
        wipes::record(&self.0);
    }
}

/// Test support: each dropped [`Wiped`] buffer's address and whether it held only zeros at the
/// end of its drop, read from the storage itself just before it is freed.
#[cfg(test)]
mod wipes {
    use std::cell::RefCell;

    thread_local! {
        static LOG: RefCell<Vec<(usize, bool)>> = const { RefCell::new(Vec::new()) };
    }

    pub(crate) fn record(bytes: &[u8]) {
        let entry = (bytes.as_ptr().addr(), bytes.iter().all(|&b| b == 0));
        LOG.with(|log| log.borrow_mut().push(entry));
    }

    pub(crate) fn clear() {
        LOG.with(|log| log.borrow_mut().clear());
    }

    /// Whether the buffer at `addr` was dropped since the last clear, and if it was zero.
    pub(crate) fn wiped(addr: usize) -> Option<bool> {
        LOG.with(|log| {
            log.borrow()
                .iter()
                .find(|(a, _)| *a == addr)
                .map(|(_, zero)| *zero)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Channel, HASH_LEN, Leg, PATTERN, prologue};

    #[test]
    fn the_prologue_bytes_are_fixed() {
        let mut control = vec![15];
        control.extend_from_slice(b"Kumochi session");
        control.extend([1, 0, 0]);
        assert_eq!(
            prologue(Channel::Release, Leg::Control),
            control,
            "release control"
        );

        let hash = [0xA5; HASH_LEN];
        let mut data = vec![15];
        data.extend_from_slice(b"Kumochi session");
        data.extend([1, 1, 1]);
        data.extend_from_slice(&hash);
        assert_eq!(prologue(Channel::Dev, Leg::Data(&hash)), data, "dev data");
    }

    #[test]
    fn the_pattern_has_no_static_key_for_either_role() {
        let params: snow::params::NoiseParams = PATTERN.parse().expect("the pattern parses");
        let pattern = params.handshake.pattern;
        assert_eq!(
            pattern.as_str(),
            "NN",
            "no s or rs token in the base pattern"
        );
        for initiator in [true, false] {
            assert!(
                !pattern.needs_local_static_key(initiator),
                "local static, {initiator}"
            );
            assert!(
                !pattern.need_known_remote_pubkey(initiator),
                "remote static, {initiator}"
            );
        }
        assert!(params.handshake.is_psk(), "the group secret is the PSK");
    }
}
