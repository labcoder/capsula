//! Both roles' handshake and binding state machines. Each state is consumed by its one step, so
//! the order is fixed by type and a refusal leaves nothing to continue with.
//!
//! The shell sends control init, control Finished, data init, data Finished, then Bind; the brain
//! answers init with its reply and accepts nothing else before Finished, Bind only after data
//! Finished, and returns the [`BoundPair`] proof only from the step that verified Bind and sealed
//! `BindAck`.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use kumochi_protocol::frame::Pipe;
use snow::{Builder, HandshakeState};
use zeroize::Zeroize;

use crate::record::{self, Opener, Sealer};
use crate::{
    Attempt, Channel, ENVELOPE_LEN, HASH_LEN, Leg, PATTERN, RECORD_LIMIT, SECRET_LEN, SessionError,
    TAG_LEN, Wiped, prologue,
};

/// An ephemeral public key and an empty payload's tag.
const HANDSHAKE_LEN: usize = 32 + TAG_LEN;
/// An empty record: Finished and `BindAck`.
const CONFIRM_LEN: usize = TAG_LEN;
/// A record carrying the data handshake hash.
const BIND_LEN: usize = HASH_LEN + TAG_LEN;

/// Bytes for one leg, to be written whole and in the order they are returned.
#[derive(Debug, PartialEq, Eq)]
pub struct Outgoing {
    /// The leg they go out on.
    pub leg: Pipe,
    /// The envelope and the message or record.
    pub bytes: Vec<u8>,
}

/// One leg's two record owners.
#[derive(Debug)]
pub struct Records {
    /// This side's outgoing direction.
    pub sealer: Sealer,
    /// This side's incoming direction.
    pub opener: Opener,
}

/// Both legs' owners, ready for application records.
#[derive(Debug)]
pub struct Session {
    /// The control leg.
    pub control: Records,
    /// The data leg.
    pub data: Records,
}

/// The brain's proof that this pair verified Bind and sealed `BindAck`. Only
/// [`BrainAwaitsBind::receive`] makes one, and it names its own pair's owners only.
pub struct BoundPair {
    attempt: Arc<Attempt>,
}

impl BoundPair {
    /// Whether `opener` belongs to the pair this proof is for.
    pub fn is_for(&self, opener: &Opener) -> bool {
        opener.belongs_to(&self.attempt)
    }
}

/// The group secret's application-owned copy, wiped when the data leg's handshake state has been
/// built from it or when the attempt ends first.
struct Keys {
    secret: Wiped,
    channel: Channel,
    #[cfg(test)]
    ephemerals: Option<[[u8; 32]; 2]>,
}

impl Keys {
    fn new(secret: &[u8; SECRET_LEN], channel: Channel) -> Self {
        let mut copy = Wiped::zeroed(SECRET_LEN);
        copy.copy_from_slice(secret);
        Self {
            secret: copy,
            channel,
            #[cfg(test)]
            ephemerals: None,
        }
    }

    fn record_len(
        &mut self,
        attempt: &Attempt,
        envelope: [u8; ENVELOPE_LEN],
        allowed: usize,
    ) -> Result<usize, SessionError> {
        attempt
            .exact(envelope, allowed)
            .inspect_err(|_| self.secret[..].zeroize())
    }

    /// The leg's handshake state. The only builder in the crate: it gets the PSK and the
    /// prologue, never a static key.
    fn handshake(&self, role: Role, leg: Leg<'_>) -> Result<HandshakeState, SessionError> {
        let params = PATTERN.parse().map_err(|_| SessionError::Handshake)?;
        let prologue = prologue(self.channel, leg);
        let psk = <&[u8; SECRET_LEN]>::try_from(&self.secret[..])
            .expect("the secret copy is SECRET_LEN bytes");
        let builder = Builder::new(params)
            .prologue(&prologue)
            .and_then(|builder| builder.psk(0, psk))
            .map_err(|_| SessionError::Handshake)?;
        #[cfg(test)]
        let builder = match &self.ephemerals {
            Some([control, data]) => builder.fixed_ephemeral_key_for_testing_only(match leg {
                Leg::Control => control,
                Leg::Data(_) => data,
            }),
            None => builder,
        };
        match role {
            Role::Shell => builder.build_initiator(),
            Role::Brain => builder.build_responder(),
        }
        .map_err(|_| SessionError::Handshake)
    }
}

/// The shell initiates; the brain responds.
#[derive(Clone, Copy)]
enum Role {
    Shell,
    Brain,
}

fn new_attempt(limit: u64) -> Arc<Attempt> {
    Arc::new(Attempt {
        retired: AtomicBool::new(false),
        limit,
    })
}

fn write_handshake(state: &mut HandshakeState, leg: Pipe) -> Result<Outgoing, SessionError> {
    let mut bytes = vec![0; ENVELOPE_LEN + HANDSHAKE_LEN];
    let written = state.write_message(&[], &mut bytes[ENVELOPE_LEN..]);
    if written != Ok(HANDSHAKE_LEN) {
        return Err(SessionError::Handshake);
    }
    let envelope = u16::try_from(HANDSHAKE_LEN).expect("a handshake message fits the envelope");
    bytes[..ENVELOPE_LEN].copy_from_slice(&envelope.to_be_bytes());
    Ok(Outgoing { leg, bytes })
}

fn read_handshake(
    state: &mut HandshakeState,
    attempt: &Attempt,
    message: &[u8],
) -> Result<[u8; HASH_LEN], SessionError> {
    attempt.check()?;
    if message.len() != HANDSHAKE_LEN {
        return attempt.fail(SessionError::Length { len: message.len() });
    }
    if state.read_message(message, &mut []).is_err() {
        return attempt.fail(SessionError::Handshake);
    }
    handshake_hash(state)
}

fn handshake_hash(state: &HandshakeState) -> Result<[u8; HASH_LEN], SessionError> {
    state
        .get_handshake_hash()
        .try_into()
        .map_err(|_| SessionError::Handshake)
}

/// The shell, after sending control init: waits for the brain's reply on control.
pub struct ShellAwaitsReply {
    control: HandshakeState,
    keys: Keys,
    attempt: Arc<Attempt>,
}

impl ShellAwaitsReply {
    /// Starts the shell's side with its copy of the group secret. Returns the control init.
    ///
    /// # Errors
    /// The Noise library could not build the handshake.
    pub fn start(
        secret: &[u8; SECRET_LEN],
        channel: Channel,
    ) -> Result<(Self, Outgoing), SessionError> {
        Self::begin(Keys::new(secret, channel), RECORD_LIMIT)
    }

    fn begin(keys: Keys, limit: u64) -> Result<(Self, Outgoing), SessionError> {
        let mut control = keys.handshake(Role::Shell, Leg::Control)?;
        let init = write_handshake(&mut control, Pipe::Control)?;
        Ok((
            Self {
                control,
                keys,
                attempt: new_attempt(limit),
            },
            init,
        ))
    }

    /// The reply's length from its envelope.
    ///
    /// # Errors
    /// Any length but the reply's, which retires the attempt and wipes its secret copy.
    pub fn record_len(&mut self, envelope: [u8; ENVELOPE_LEN]) -> Result<usize, SessionError> {
        self.keys.record_len(&self.attempt, envelope, HANDSHAKE_LEN)
    }

    /// Takes the control reply. Returns control Finished, then data init, in that order.
    ///
    /// # Errors
    /// A wrong length or a reply that fails the handshake.
    pub fn receive(
        self,
        reply: &[u8],
    ) -> Result<(ShellAwaitsDataReply, [Outgoing; 2]), SessionError> {
        let Self {
            mut control,
            keys,
            attempt,
        } = self;
        let control_hash = read_handshake(&mut control, &attempt, reply)?;
        let (mut sealer, opener) = record::split(control, Pipe::Control, &attempt)?;
        let finished = sealer.seal_out(&[])?;
        let mut data = keys.handshake(Role::Shell, Leg::Data(&control_hash))?;
        drop(keys);
        let init = write_handshake(&mut data, Pipe::Data)?;
        let next = ShellAwaitsDataReply {
            data,
            control: Records { sealer, opener },
            attempt,
        };
        Ok((
            next,
            [
                Outgoing {
                    leg: Pipe::Control,
                    bytes: finished,
                },
                init,
            ],
        ))
    }
}

/// The shell, after data init: waits for the brain's reply on data.
pub struct ShellAwaitsDataReply {
    data: HandshakeState,
    control: Records,
    attempt: Arc<Attempt>,
}

impl ShellAwaitsDataReply {
    /// The reply's length from its envelope.
    ///
    /// # Errors
    /// Any length but the reply's, which retires the attempt.
    pub fn record_len(&self, envelope: [u8; ENVELOPE_LEN]) -> Result<usize, SessionError> {
        self.attempt.exact(envelope, HANDSHAKE_LEN)
    }

    /// Takes the data reply. Returns data Finished, then Bind on control, in that order.
    ///
    /// # Errors
    /// A wrong length, a reply that fails the handshake, or an earlier failure.
    pub fn receive(
        self,
        reply: &[u8],
    ) -> Result<(ShellAwaitsBindAck, [Outgoing; 2]), SessionError> {
        let Self {
            mut data,
            mut control,
            attempt,
        } = self;
        let data_hash = read_handshake(&mut data, &attempt, reply)?;
        let (mut sealer, opener) = record::split(data, Pipe::Data, &attempt)?;
        let finished = sealer.seal_out(&[])?;
        let bind = control.sealer.seal_out(&data_hash)?;
        let session = Session {
            control,
            data: Records { sealer, opener },
        };
        Ok((
            ShellAwaitsBindAck { session, attempt },
            [
                Outgoing {
                    leg: Pipe::Data,
                    bytes: finished,
                },
                Outgoing {
                    leg: Pipe::Control,
                    bytes: bind,
                },
            ],
        ))
    }
}

/// The shell, after Bind: waits for `BindAck` on control. It has no owners to use until then.
pub struct ShellAwaitsBindAck {
    session: Session,
    attempt: Arc<Attempt>,
}

impl ShellAwaitsBindAck {
    /// `BindAck`'s length from its envelope.
    ///
    /// # Errors
    /// Any length but `BindAck`'s, which retires the attempt.
    pub fn record_len(&self, envelope: [u8; ENVELOPE_LEN]) -> Result<usize, SessionError> {
        self.attempt.exact(envelope, CONFIRM_LEN)
    }

    /// Verifies `BindAck` and returns the pair's owners for application records.
    ///
    /// # Errors
    /// A wrong length, a record that fails to authenticate, or an earlier failure.
    pub fn receive(mut self, ack: &[u8]) -> Result<Session, SessionError> {
        self.session.control.opener.open_exact(ack, &[])?;
        Ok(self.session)
    }
}

/// The brain, before anything: waits for control init.
pub struct BrainAwaitsInit {
    control: HandshakeState,
    keys: Keys,
    attempt: Arc<Attempt>,
}

impl BrainAwaitsInit {
    /// Starts the brain's side with its copy of the group secret.
    ///
    /// # Errors
    /// The Noise library could not build the handshake.
    pub fn new(secret: &[u8; SECRET_LEN], channel: Channel) -> Result<Self, SessionError> {
        Self::begin(Keys::new(secret, channel), RECORD_LIMIT)
    }

    fn begin(keys: Keys, limit: u64) -> Result<Self, SessionError> {
        let control = keys.handshake(Role::Brain, Leg::Control)?;
        Ok(Self {
            control,
            keys,
            attempt: new_attempt(limit),
        })
    }

    /// Init's length from its envelope.
    ///
    /// # Errors
    /// Any length but init's, which retires the attempt and wipes its secret copy.
    pub fn record_len(&mut self, envelope: [u8; ENVELOPE_LEN]) -> Result<usize, SessionError> {
        self.keys.record_len(&self.attempt, envelope, HANDSHAKE_LEN)
    }

    /// Takes control init and returns the reply, the only thing sent before Finished.
    ///
    /// # Errors
    /// A wrong length or an init that fails the handshake: another secret, channel or leg.
    pub fn receive(self, init: &[u8]) -> Result<(BrainAwaitsFinished, Outgoing), SessionError> {
        let Self {
            mut control,
            keys,
            attempt,
        } = self;
        read_handshake(&mut control, &attempt, init)?;
        let reply = write_handshake(&mut control, Pipe::Control)?;
        let control_hash = handshake_hash(&control)?;
        let (sealer, opener) = record::split(control, Pipe::Control, &attempt)?;
        let next = BrainAwaitsFinished {
            control: Records { sealer, opener },
            control_hash,
            keys,
            attempt,
        };
        Ok((next, reply))
    }
}

/// The brain, after its control reply: waits for control Finished.
pub struct BrainAwaitsFinished {
    control: Records,
    control_hash: [u8; HASH_LEN],
    keys: Keys,
    attempt: Arc<Attempt>,
}

impl BrainAwaitsFinished {
    /// Finished's length from its envelope.
    ///
    /// # Errors
    /// Any length but Finished's, which retires the attempt and wipes its secret copy.
    pub fn record_len(&mut self, envelope: [u8; ENVELOPE_LEN]) -> Result<usize, SessionError> {
        self.keys.record_len(&self.attempt, envelope, CONFIRM_LEN)
    }

    /// Verifies control Finished; the data leg's handshake starts only after it.
    ///
    /// # Errors
    /// A wrong length, a record that fails to authenticate, or an earlier failure.
    pub fn receive(self, finished: &[u8]) -> Result<BrainAwaitsDataInit, SessionError> {
        let Self {
            mut control,
            control_hash,
            keys,
            attempt,
        } = self;
        control.opener.open_exact(finished, &[])?;
        let data = keys.handshake(Role::Brain, Leg::Data(&control_hash))?;
        drop(keys);
        Ok(BrainAwaitsDataInit {
            data,
            control,
            attempt,
        })
    }
}

/// The brain, after control Finished: waits for data init.
pub struct BrainAwaitsDataInit {
    data: HandshakeState,
    control: Records,
    attempt: Arc<Attempt>,
}

impl BrainAwaitsDataInit {
    /// Data init's length from its envelope.
    ///
    /// # Errors
    /// Any length but init's, which retires the attempt.
    pub fn record_len(&self, envelope: [u8; ENVELOPE_LEN]) -> Result<usize, SessionError> {
        self.attempt.exact(envelope, HANDSHAKE_LEN)
    }

    /// Takes data init and returns the data reply.
    ///
    /// # Errors
    /// A wrong length, an init that fails the handshake (another control hash included), or an
    /// earlier failure.
    pub fn receive(self, init: &[u8]) -> Result<(BrainAwaitsDataFinished, Outgoing), SessionError> {
        let Self {
            mut data,
            control,
            attempt,
        } = self;
        read_handshake(&mut data, &attempt, init)?;
        let reply = write_handshake(&mut data, Pipe::Data)?;
        let data_hash = handshake_hash(&data)?;
        let (sealer, opener) = record::split(data, Pipe::Data, &attempt)?;
        let next = BrainAwaitsDataFinished {
            control,
            data: Records { sealer, opener },
            data_hash,
            attempt,
        };
        Ok((next, reply))
    }
}

/// The brain, after its data reply: waits for data Finished.
pub struct BrainAwaitsDataFinished {
    control: Records,
    data: Records,
    data_hash: [u8; HASH_LEN],
    attempt: Arc<Attempt>,
}

impl BrainAwaitsDataFinished {
    /// Finished's length from its envelope.
    ///
    /// # Errors
    /// Any length but Finished's, which retires the attempt.
    pub fn record_len(&self, envelope: [u8; ENVELOPE_LEN]) -> Result<usize, SessionError> {
        self.attempt.exact(envelope, CONFIRM_LEN)
    }

    /// Verifies data Finished; Bind is accepted only after it.
    ///
    /// # Errors
    /// A wrong length, a record that fails to authenticate, or an earlier failure.
    pub fn receive(self, finished: &[u8]) -> Result<BrainAwaitsBind, SessionError> {
        let Self {
            control,
            mut data,
            data_hash,
            attempt,
        } = self;
        data.opener.open_exact(finished, &[])?;
        Ok(BrainAwaitsBind {
            control,
            data,
            data_hash,
            attempt,
        })
    }
}

/// The brain, after data Finished: waits for Bind on control.
pub struct BrainAwaitsBind {
    control: Records,
    data: Records,
    data_hash: [u8; HASH_LEN],
    attempt: Arc<Attempt>,
}

impl BrainAwaitsBind {
    /// Bind's length from its envelope.
    ///
    /// # Errors
    /// Any length but Bind's, which retires the attempt.
    pub fn record_len(&self, envelope: [u8; ENVELOPE_LEN]) -> Result<usize, SessionError> {
        self.attempt.exact(envelope, BIND_LEN)
    }

    /// Verifies that Bind names this pair's data handshake and seals `BindAck`. Returns the owners,
    /// the proof and `BindAck`, which goes out before any Hello is read.
    ///
    /// # Errors
    /// A wrong length, a record that fails to authenticate, another data hash, or an earlier
    /// failure.
    pub fn receive(self, bind: &[u8]) -> Result<(Session, BoundPair, Outgoing), SessionError> {
        let Self {
            mut control,
            data,
            data_hash,
            attempt,
        } = self;
        control.opener.open_exact(bind, &data_hash)?;
        let ack = control.sealer.seal_out(&[])?;
        let proof = BoundPair { attempt };
        Ok((
            Session { control, data },
            proof,
            Outgoing {
                leg: Pipe::Control,
                bytes: ack,
            },
        ))
    }
}

macro_rules! opaque_debug {
    ($($state:ident),+) => {$(
        impl fmt::Debug for $state {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(stringify!($state))
            }
        }
    )+};
}

opaque_debug!(
    BoundPair,
    ShellAwaitsReply,
    ShellAwaitsDataReply,
    ShellAwaitsBindAck,
    BrainAwaitsInit,
    BrainAwaitsFinished,
    BrainAwaitsDataInit,
    BrainAwaitsDataFinished,
    BrainAwaitsBind
);

#[cfg(test)]
pub(crate) mod tests {
    use super::{
        BoundPair, BrainAwaitsBind, BrainAwaitsDataFinished, BrainAwaitsDataInit,
        BrainAwaitsFinished, BrainAwaitsInit, Keys, Outgoing, Session, ShellAwaitsBindAck,
        ShellAwaitsDataReply, ShellAwaitsReply,
    };
    use crate::record::{Opener, Sealer};
    use crate::{Channel, ENVELOPE_LEN, RECORD_LIMIT, SessionError, wipes};
    use kumochi_protocol::frame::Pipe;
    use std::fmt::Write as _;

    const SHELL_E: [[u8; 32]; 2] = [[0x11; 32], [0x12; 32]];
    const BRAIN_E: [[u8; 32]; 2] = [[0x21; 32], [0x22; 32]];

    // From an independent Python implementation of Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s, written
    // from the Noise specification and the wire rules rather than from snow or this crate: secret
    // 00..1f, Release, the ephemerals above, and the frame [1, 0, 0, 0, x] as the first
    // application record of each direction.
    const CONTROL_INIT: &str = "00307b4e909bbe7ffe44c465a220037d608ee35897d31ef972f07f74892cb0f73f1380faddd4c7e1607fc0864429caf90966";
    const CONTROL_REPLY: &str = "00307d34a4815fa6b982535e60af3bd9b49556816080f1641ff81d2b7c8ae8268a442fdf3b784ed5d5dcad6319902bfa5095";
    const CONTROL_FINISHED: &str = "0010aec20ac841bc3d0a84fa944a226905bb";
    const DATA_INIT: &str = "0030052a50773ac8d91773f2dc9662e12f0defe915e415b8a1c8e20a5a3d6ab2b843128f72623b5f137687605454ff96699f";
    const DATA_REPLY: &str = "00300faa684ed28867b97f4a6a2dee5df8ce974e76b7018e3f22a1c4cf2678570f20afbb9493907207ea456cde4561a49c94";
    const DATA_FINISHED: &str = "0010e3bd85b666733c87b5944c903b25e934";
    const BIND: &str = "0030f1407ad36191dd84a75d38e7dc6721417ccd015847d48460a8c7ee7f918ba4007163e24e401fa47e1792749c8a23e09a";
    const BIND_ACK: &str = "0010dafe6a109436dbb4da7287e83be27736";
    const APP_CONTROL_SHELL: &str = "00152fdc7bdbd6a77fd3fe0ae608946caca5aa3baa7fd2";
    const APP_CONTROL_BRAIN: &str = "00153f6a0dbb38c58dd779a8338251bb2cc00300b23204";
    const APP_DATA_SHELL: &str = "0015537769cb4eb56065c53d63e94faddcd2c585f951ad";
    const APP_DATA_BRAIN: &str = "0015da8ed2b3381a6a80b1d23f0b2d8b974bb447777fb5";

    fn secret() -> [u8; 32] {
        std::array::from_fn(|i| u8::try_from(i).expect("under 32"))
    }

    fn keys(secret: &[u8; 32], channel: Channel, ephemerals: [[u8; 32]; 2]) -> Keys {
        Keys {
            ephemerals: Some(ephemerals),
            ..Keys::new(secret, channel)
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().fold(String::new(), |mut out, b| {
            write!(out, "{b:02x}").expect("writing to a String");
            out
        })
    }

    fn body(out: &Outgoing) -> &[u8] {
        &out.bytes[ENVELOPE_LEN..]
    }

    pub(crate) fn envelope(len: usize) -> [u8; 2] {
        u16::try_from(len).expect("a u16 length").to_be_bytes()
    }

    pub(crate) struct Run {
        pub(crate) shell: Session,
        pub(crate) brain: Session,
        pub(crate) proof: BoundPair,
        pub(crate) transcript: Vec<Outgoing>,
        control_hash: [u8; 32],
        data_hash: [u8; 32],
    }

    /// The honest exchange, each message's envelope read through its state's `record_len`.
    fn run(shell: Keys, brain: Keys, limit: u64) -> Result<Run, SessionError> {
        let (mut s, init) = ShellAwaitsReply::begin(shell, limit)?;
        let mut b = BrainAwaitsInit::begin(brain, limit)?;
        b.record_len(envelope(body(&init).len()))?;
        let (mut b, reply) = b.receive(body(&init))?;
        let control_hash = b.control_hash;
        s.record_len(envelope(body(&reply).len()))?;
        let (s, [finished, data_init]) = s.receive(body(&reply))?;
        b.record_len(envelope(body(&finished).len()))?;
        let b = b.receive(body(&finished))?;
        b.record_len(envelope(body(&data_init).len()))?;
        let (b, data_reply) = b.receive(body(&data_init))?;
        let data_hash = b.data_hash;
        s.record_len(envelope(body(&data_reply).len()))?;
        let (s, [data_finished, bind]) = s.receive(body(&data_reply))?;
        b.record_len(envelope(body(&data_finished).len()))?;
        let b = b.receive(body(&data_finished))?;
        b.record_len(envelope(body(&bind).len()))?;
        let (brain, proof, ack) = b.receive(body(&bind))?;
        s.record_len(envelope(body(&ack).len()))?;
        let shell = s.receive(body(&ack))?;
        let transcript = vec![
            init,
            reply,
            finished,
            data_init,
            data_reply,
            data_finished,
            bind,
            ack,
        ];
        Ok(Run {
            shell,
            brain,
            proof,
            transcript,
            control_hash,
            data_hash,
        })
    }

    /// The fixed-ephemeral pair with `limit` records per direction.
    pub(crate) fn fixed(limit: u64) -> Result<Run, SessionError> {
        let shell = keys(&secret(), Channel::Release, SHELL_E);
        run(shell, keys(&secret(), Channel::Release, BRAIN_E), limit)
    }

    pub(crate) fn established() -> Run {
        fixed(RECORD_LIMIT).expect("the honest pair establishes")
    }

    /// A pair with the library's own random ephemerals.
    fn fresh() -> Run {
        let shell = Keys::new(&secret(), Channel::Release);
        run(shell, Keys::new(&secret(), Channel::Release), RECORD_LIMIT).expect("establishes")
    }

    fn honest() -> Vec<Outgoing> {
        established().transcript
    }

    // Each role at each phase, reached by replaying the deterministic honest transcript:
    // 0 init, 1 reply, 2 Finished, 3 data init, 4 data reply, 5 data Finished, 6 Bind, 7 BindAck.
    fn shell_reply() -> ShellAwaitsReply {
        let shell = keys(&secret(), Channel::Release, SHELL_E);
        ShellAwaitsReply::begin(shell, RECORD_LIMIT)
            .expect("starts")
            .0
    }
    fn shell_data_reply() -> ShellAwaitsDataReply {
        shell_reply().receive(body(&honest()[1])).expect("reply").0
    }
    fn shell_bind_ack() -> ShellAwaitsBindAck {
        shell_data_reply()
            .receive(body(&honest()[4]))
            .expect("data reply")
            .0
    }
    fn brain_init() -> BrainAwaitsInit {
        let brain = keys(&secret(), Channel::Release, BRAIN_E);
        BrainAwaitsInit::begin(brain, RECORD_LIMIT).expect("starts")
    }
    fn brain_finished() -> BrainAwaitsFinished {
        brain_init().receive(body(&honest()[0])).expect("init").0
    }
    fn brain_data_init() -> BrainAwaitsDataInit {
        brain_finished()
            .receive(body(&honest()[2]))
            .expect("finished")
    }
    fn brain_data_finished() -> BrainAwaitsDataFinished {
        brain_data_init()
            .receive(body(&honest()[3]))
            .expect("data init")
            .0
    }
    fn brain_bind() -> BrainAwaitsBind {
        brain_data_finished()
            .receive(body(&honest()[5]))
            .expect("data finished")
    }

    fn deliver(sealer: &mut Sealer, opener: &mut Opener, frame: &[u8]) -> Result<(), SessionError> {
        sealer.seal(frame)?;
        let record = sealer.unwritten()?[ENVELOPE_LEN..].to_vec();
        sealer.advance(ENVELOPE_LEN + record.len())?;
        opener.record_len(envelope(record.len()))?;
        opener.open(&record).map(drop)
    }

    #[test]
    fn the_transcript_matches_the_independent_vectors_for_every_role_and_phase() {
        let mut run = established();
        let expected = [
            (Pipe::Control, CONTROL_INIT),
            (Pipe::Control, CONTROL_REPLY),
            (Pipe::Control, CONTROL_FINISHED),
            (Pipe::Data, DATA_INIT),
            (Pipe::Data, DATA_REPLY),
            (Pipe::Data, DATA_FINISHED),
            (Pipe::Control, BIND),
            (Pipe::Control, BIND_ACK),
        ];
        for (i, (out, (leg, bytes))) in run.transcript.iter().zip(expected).enumerate() {
            assert_eq!(
                (out.leg, hex(&out.bytes)),
                (leg, bytes.to_owned()),
                "message {i}"
            );
        }

        let cases = [
            (
                &mut run.shell.control.sealer,
                &mut run.brain.control.opener,
                0xC1,
                APP_CONTROL_SHELL,
            ),
            (
                &mut run.brain.control.sealer,
                &mut run.shell.control.opener,
                0xC2,
                APP_CONTROL_BRAIN,
            ),
            (
                &mut run.shell.data.sealer,
                &mut run.brain.data.opener,
                0xD1,
                APP_DATA_SHELL,
            ),
            (
                &mut run.brain.data.sealer,
                &mut run.shell.data.opener,
                0xD2,
                APP_DATA_BRAIN,
            ),
        ];
        for (sealer, opener, marker, record) in cases {
            let frame = [1, 0, 0, 0, marker];
            sealer.seal(&frame).expect("seals");
            let sealed = sealer.unwritten().expect("sealed bytes");
            assert_eq!(hex(sealed), record, "record {marker:#x}");
            opener
                .record_len(envelope(frame.len() + 16))
                .expect("length");
            let delivery = opener.open(&sealed[ENVELOPE_LEN..]).expect("opens");
            assert_eq!(
                (delivery.bytes(), delivery.frame_done()),
                (&frame[..], true),
                "{marker:#x}"
            );
        }
    }

    #[test]
    fn the_first_application_nonces_continue_from_the_confirmation_and_binding_records() {
        let run = established();
        let positions = [
            (
                run.shell.control.sealer.next(),
                run.brain.control.opener.next(),
            ),
            (
                run.brain.control.sealer.next(),
                run.shell.control.opener.next(),
            ),
            (run.shell.data.sealer.next(), run.brain.data.opener.next()),
            (run.brain.data.sealer.next(), run.shell.data.opener.next()),
        ];
        assert_eq!(
            positions,
            [(2, 2), (1, 1), (1, 1), (0, 0)],
            "control s>b, b>s, data s>b, b>s"
        );
    }

    #[test]
    fn a_counter_reset_at_either_single_end_fails_to_authenticate() {
        for (direction, reset_sealer) in (0..4).flat_map(|d| [(d, true), (d, false)]) {
            let mut run = established();
            let (sealer, opener) = match direction {
                0 => (&mut run.shell.control.sealer, &mut run.brain.control.opener),
                1 => (&mut run.brain.control.sealer, &mut run.shell.control.opener),
                2 => (&mut run.shell.data.sealer, &mut run.brain.data.opener),
                _ => (&mut run.brain.data.sealer, &mut run.shell.data.opener),
            };
            deliver(sealer, opener, &[0, 0, 0, 0]).expect("the first record opens");
            if reset_sealer {
                sealer.reset_for_test();
            } else {
                opener.reset_for_test();
            }
            assert_eq!(
                deliver(sealer, opener, &[0, 0, 0, 0]),
                Err(SessionError::Authentication),
                "direction {direction}, sealer reset {reset_sealer}"
            );
        }
    }

    #[test]
    fn two_pairs_have_fresh_ephemerals_and_handshake_hashes() {
        let (one, two) = (fresh(), fresh());
        for i in [0, 1, 3, 4] {
            assert_ne!(
                one.transcript[i].bytes[2..34],
                two.transcript[i].bytes[2..34],
                "ephemeral in message {i}"
            );
        }
        assert_ne!(
            one.control_hash, two.control_hash,
            "control handshake hashes"
        );
        assert_ne!(one.data_hash, two.data_hash, "data handshake hashes");
    }

    #[test]
    fn a_wrong_secret_is_refused_by_either_role_before_any_record() {
        let other = [0xEE; 32];
        let brain = BrainAwaitsInit::begin(keys(&other, Channel::Release, BRAIN_E), RECORD_LIMIT);
        let refused = brain.expect("starts").receive(body(&honest()[0])).err();
        assert_eq!(
            refused,
            Some(SessionError::Handshake),
            "the brain refuses the shell's init"
        );

        let (_, impostor_init) =
            ShellAwaitsReply::begin(keys(&other, Channel::Release, SHELL_E), RECORD_LIMIT)
                .expect("starts");
        let impostor =
            BrainAwaitsInit::begin(keys(&other, Channel::Release, BRAIN_E), RECORD_LIMIT);
        let (_, reply) = impostor
            .expect("starts")
            .receive(body(&impostor_init))
            .expect("init");
        let refused = shell_reply().receive(body(&reply)).err();
        assert_eq!(
            refused,
            Some(SessionError::Handshake),
            "the shell refuses the brain's reply"
        );
    }

    #[test]
    fn a_wrong_channel_leg_or_control_hash_is_refused() {
        let dev = keys(&secret(), Channel::Dev, SHELL_E);
        let (_, dev_init) = ShellAwaitsReply::begin(dev, RECORD_LIMIT).expect("starts");
        let refused = brain_init().receive(body(&dev_init)).err();
        assert_eq!(refused, Some(SessionError::Handshake), "channel");

        let refused = brain_init().receive(body(&honest()[3])).err();
        assert_eq!(
            refused,
            Some(SessionError::Handshake),
            "data init on control"
        );
        let refused = brain_data_init().receive(body(&honest()[0])).err();
        assert_eq!(
            refused,
            Some(SessionError::Handshake),
            "control init on data"
        );

        let other = run(
            keys(&secret(), Channel::Release, [[0x31; 32], [0x12; 32]]),
            keys(&secret(), Channel::Release, BRAIN_E),
            RECORD_LIMIT,
        )
        .expect("establishes");
        let refused = brain_data_init().receive(body(&other.transcript[3])).err();
        assert_eq!(
            refused,
            Some(SessionError::Handshake),
            "data init bound to another control leg"
        );
    }

    macro_rules! check_lengths {
        ($state:expr, $allowed:expr, $valid:expr $(, $mutability:ident)?) => {{
            let allowed: usize = $allowed;
            for wrong in [0, 15, allowed - 1, allowed + 1, 65_535] {
                let $($mutability)? state = $state;
                assert_eq!(
                    state.record_len(envelope(wrong)),
                    Err(SessionError::Length { len: wrong })
                );
                assert_eq!(
                    state.receive($valid).err(),
                    Some(SessionError::Retired),
                    "after {wrong}"
                );
            }
            assert_eq!($state.record_len(envelope(allowed)), Ok(allowed), "allowed");
            let short = &$valid[..allowed - 1];
            assert_eq!(
                $state.receive(short).err(),
                Some(SessionError::Length { len: allowed - 1 })
            );
        }};
    }

    #[test]
    fn every_other_length_in_every_handshake_phase_is_refused_and_retires_the_attempt() {
        let t = honest();
        check_lengths!(shell_reply(), 48, body(&t[1]), mut);
        check_lengths!(shell_data_reply(), 48, body(&t[4]));
        check_lengths!(shell_bind_ack(), 16, body(&t[7]));
        check_lengths!(brain_init(), 48, body(&t[0]), mut);
        check_lengths!(brain_finished(), 16, body(&t[2]), mut);
        check_lengths!(brain_data_init(), 48, body(&t[3]));
        check_lengths!(brain_data_finished(), 16, body(&t[5]));
        check_lengths!(brain_bind(), 48, body(&t[6]));
    }

    #[test]
    fn a_replayed_init_gets_a_fresh_reply_but_cannot_reuse_finished() {
        let (shell, init) = ShellAwaitsReply::start(&secret(), Channel::Release).expect("starts");
        let reply = || {
            BrainAwaitsInit::new(&secret(), Channel::Release)
                .expect("starts")
                .receive(body(&init))
                .expect("both an original and a replay get message 2")
        };
        let (original, first) = reply();
        let (replayed, second) = reply();
        assert_eq!(second.leg, Pipe::Control);
        assert_eq!(&second.bytes[..2], &[0, 48], "a message 2 envelope");
        assert_ne!(
            body(&first)[..32],
            body(&second)[..32],
            "fresh responder ephemeral"
        );
        let (_, [finished, _]) = shell
            .receive(body(&first))
            .expect("first reply authenticates");
        let refused = replayed.receive(body(&finished)).err();
        assert_eq!(
            refused,
            Some(SessionError::Authentication),
            "stale Finished on a fresh attempt"
        );
        let refused = original.receive(body(&init)).err();
        assert_eq!(
            refused,
            Some(SessionError::Length { len: 48 }),
            "a second init on the pair"
        );
    }

    #[test]
    fn a_refused_envelope_wipes_the_live_secret_in_all_three_states() {
        macro_rules! check {
            ($state:expr, $allowed:expr) => {{
                let mut state = $state;
                assert_eq!(state.record_len(envelope($allowed)), Ok($allowed));
                assert_eq!(
                    &state.keys.secret[..],
                    &secret(),
                    "valid envelope keeps the key"
                );
                assert_eq!(
                    state.record_len(envelope(15)),
                    Err(SessionError::Length { len: 15 })
                );
                assert_eq!(
                    &state.keys.secret[..],
                    &[0; 32],
                    "wiped before state is dropped"
                );
                assert_eq!(
                    state.record_len(envelope($allowed)),
                    Err(SessionError::Retired)
                );
            }};
        }
        check!(shell_reply(), 48);
        check!(brain_init(), 48);
        check!(brain_finished(), 16);
    }

    #[test]
    fn repeated_early_or_reflected_phase_messages_are_refused() {
        let t = honest();
        let refused = brain_finished().receive(body(&t[6])).err();
        assert_eq!(
            refused,
            Some(SessionError::Length { len: 48 }),
            "Bind before Finished"
        );
        let refused = brain_data_finished().receive(body(&t[2])).err();
        assert_eq!(
            refused,
            Some(SessionError::Authentication),
            "control Finished on data"
        );
        let refused = brain_bind().receive(body(&t[2])).err();
        assert_eq!(
            refused,
            Some(SessionError::Length { len: 16 }),
            "Finished again for Bind"
        );

        let refused = shell_bind_ack().receive(body(&t[2])).err();
        assert_eq!(
            refused,
            Some(SessionError::Authentication),
            "own Finished reflected"
        );
        let refused = shell_bind_ack().receive(body(&t[5])).err();
        assert_eq!(
            refused,
            Some(SessionError::Authentication),
            "data record as BindAck"
        );
        let refused = shell_bind_ack().receive(body(&t[6])).err();
        assert_eq!(
            refused,
            Some(SessionError::Length { len: 48 }),
            "own Bind reflected"
        );

        let mut run = established();
        run.brain
            .control
            .opener
            .record_len(envelope(48))
            .expect("length");
        let replayed = run.brain.control.opener.open(body(&t[6])).err();
        assert_eq!(
            replayed,
            Some(SessionError::Authentication),
            "Bind replayed after binding"
        );
    }

    #[test]
    fn bind_naming_another_data_handshake_is_refused() {
        let mut shell = shell_data_reply();
        let forged = shell.control.sealer.seal_out(&[0xB1; 32]).expect("seals");
        let refused = brain_bind().receive(&forged[ENVELOPE_LEN..]).err();
        assert_eq!(refused, Some(SessionError::Bind));
    }

    #[test]
    fn the_proof_names_its_own_pair_only() {
        let (run, other) = (established(), established());
        assert!(run.proof.is_for(&run.brain.control.opener), "own control");
        assert!(run.proof.is_for(&run.brain.data.opener), "own data");
        assert!(
            !run.proof.is_for(&other.brain.control.opener),
            "another pair"
        );
    }

    #[test]
    fn the_record_limit_counts_confirmation_and_binding_records() {
        assert_eq!(
            fixed(1).err(),
            Some(SessionError::Exhausted),
            "Bind is the second record"
        );
        let mut run = fixed(4).expect("establishes");
        let (sealer, opener) = (&mut run.shell.control.sealer, &mut run.brain.control.opener);
        deliver(sealer, opener, &[0, 0, 0, 0]).expect("nonce 2");
        deliver(sealer, opener, &[0, 0, 0, 0]).expect("nonce 3, the last allowed");
        assert_eq!(
            sealer.seal(&[0, 0, 0, 0]),
            Err(SessionError::Exhausted),
            "nonce 4"
        );

        let mut run = fixed(4).expect("establishes");
        let (sealer, opener) = (&mut run.brain.data.sealer, &mut run.shell.data.opener);
        for nonce in 0..4 {
            deliver(sealer, opener, &[0, 0, 0, 0]).unwrap_or_else(|e| panic!("{nonce}: {e}"));
        }
        opener.record_len(envelope(20)).expect("length");
        assert_eq!(
            opener.open(&[0; 20]).err(),
            Some(SessionError::Exhausted),
            "open past it"
        );
        assert_eq!(
            sealer.seal(&[0, 0, 0, 0]),
            Err(SessionError::Exhausted),
            "seal past it"
        );
        let after = run.shell.control.sealer.seal(&[0, 0, 0, 0]);
        assert_eq!(
            after,
            Err(SessionError::Retired),
            "the shell's whole attempt is retired"
        );
    }

    #[test]
    fn the_secret_copy_is_wiped_after_the_data_handshake_is_built() {
        let shell = shell_reply();
        let addr = shell.keys.secret.as_ptr().addr();
        wipes::clear();
        let (shell, _) = shell.receive(body(&honest()[1])).expect("reply");
        assert_eq!(wipes::wiped(addr), Some(true), "shell");
        drop(shell);

        let brain = brain_init();
        let addr = brain.keys.secret.as_ptr().addr();
        wipes::clear();
        let (brain, _) = brain.receive(body(&honest()[0])).expect("init");
        assert_eq!(wipes::wiped(addr), None, "kept until the data leg is built");
        let brain = brain.receive(body(&honest()[2])).expect("finished");
        assert_eq!(wipes::wiped(addr), Some(true), "brain");
        drop(brain);
    }

    #[test]
    fn the_secret_copy_is_wiped_on_refusal_and_on_drop() {
        let shell = shell_reply();
        let addr = shell.keys.secret.as_ptr().addr();
        wipes::clear();
        assert_eq!(shell.receive(&[0; 48]).err(), Some(SessionError::Handshake));
        assert_eq!(wipes::wiped(addr), Some(true), "shell refusal");

        let brain = brain_finished();
        let addr = brain.keys.secret.as_ptr().addr();
        wipes::clear();
        assert_eq!(
            brain.receive(&[0; 16]).err(),
            Some(SessionError::Authentication)
        );
        assert_eq!(wipes::wiped(addr), Some(true), "brain refusal");

        for cancelled in [0, 1] {
            let addr = if cancelled == 0 {
                let state = shell_reply();
                let addr = state.keys.secret.as_ptr().addr();
                wipes::clear();
                drop(state);
                addr
            } else {
                let state = brain_init();
                let addr = state.keys.secret.as_ptr().addr();
                wipes::clear();
                drop(state);
                addr
            };
            assert_eq!(wipes::wiped(addr), Some(true), "dropped state {cancelled}");
        }
    }
}
