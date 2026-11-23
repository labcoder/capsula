//! The record owners: one [`Sealer`] and one [`Opener`] per direction of each leg. Only
//! [`split`] makes them, when a leg's handshake ends, so every counter starts there and runs on
//! through the confirmation and binding records into application use.

use std::fmt;
use std::sync::Arc;

use kumochi_protocol::frame::{self, HEADER_LEN, Pipe};
use snow::{HandshakeState, StatelessTransportState};
use zeroize::Zeroize;

use crate::{Attempt, ENVELOPE_LEN, MAX_PLAINTEXT, MAX_RECORD, SessionError, TAG_LEN, Wiped};

/// Splits a finished handshake into the leg's two owners, both at nonce 0.
pub(crate) fn split(
    handshake: HandshakeState,
    pipe: Pipe,
    attempt: &Arc<Attempt>,
) -> Result<(Sealer, Opener), SessionError> {
    let Ok(state) = handshake.into_stateless_transport_mode() else {
        return attempt.fail(SessionError::Handshake);
    };
    let state = Arc::new(state);
    let sealer = Sealer {
        state: Arc::clone(&state),
        attempt: Arc::clone(attempt),
        next: 0,
        record: Wiped::zeroed(ENVELOPE_LEN + MAX_RECORD),
        len: 0,
        written: 0,
    };
    let opener = Opener {
        state,
        attempt: Arc::clone(attempt),
        next: 0,
        scratch: Wiped::zeroed(MAX_PLAINTEXT),
        pipe,
        announced: None,
        header: [0; HEADER_LEN],
        have: 0,
        remaining: 0,
    };
    Ok((sealer, opener))
}

/// Seals one direction of a leg. A record is sealed once; a short write resumes from
/// [`Sealer::unwritten`] instead of sealing again.
pub struct Sealer {
    state: Arc<StatelessTransportState>,
    attempt: Arc<Attempt>,
    next: u64,
    record: Wiped,
    len: usize,
    written: usize,
}

impl Sealer {
    /// Seals one application record of 1 to [`MAX_PLAINTEXT`] bytes. A frame goes out as
    /// `frame.chunks(MAX_PLAINTEXT)`, one record per chunk.
    ///
    /// # Errors
    /// Empty or oversized plaintext, a record still unwritten, the record limit, or an earlier
    /// failure of the attempt. Every error retires the attempt.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<(), SessionError> {
        self.attempt.check()?;
        if plaintext.is_empty() || plaintext.len() > MAX_PLAINTEXT {
            return self.attempt.fail(SessionError::Length {
                len: plaintext.len() + TAG_LEN,
            });
        }
        self.seal_any(plaintext)
    }

    /// The envelope and ciphertext not yet written; empty when the last record is out.
    pub fn unwritten(&self) -> &[u8] {
        &self.record[self.written..self.len]
    }

    /// Marks `n` more bytes of [`Sealer::unwritten`] as accepted by the transport.
    pub fn advance(&mut self, n: usize) {
        self.written = self.written.saturating_add(n).min(self.len);
    }

    /// Seals a record of any allowed length, confirmation and binding records included.
    fn seal_any(&mut self, plaintext: &[u8]) -> Result<(), SessionError> {
        self.attempt.check()?;
        if self.written < self.len {
            return self.attempt.fail(SessionError::Pending);
        }
        if self.next >= self.attempt.limit {
            return self.attempt.fail(SessionError::Exhausted);
        }
        let body = &mut self.record[ENVELOPE_LEN..];
        let Ok(n) = self.state.write_message(self.next, plaintext, body) else {
            return self.attempt.fail(SessionError::Cipher);
        };
        let envelope = u16::try_from(n).expect("a sealed record is at most MAX_RECORD bytes");
        self.record[..ENVELOPE_LEN].copy_from_slice(&envelope.to_be_bytes());
        self.next += 1;
        self.len = ENVELOPE_LEN + n;
        self.written = 0;
        Ok(())
    }

    /// Seals a handshake-phase record and hands its bytes to the caller to write whole.
    pub(crate) fn seal_out(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, SessionError> {
        self.seal_any(plaintext)?;
        let bytes = self.unwritten().to_vec();
        self.written = self.len;
        Ok(bytes)
    }

    #[cfg(test)]
    pub(crate) fn next(&self) -> u64 {
        self.next
    }

    #[cfg(test)]
    pub(crate) fn reset_for_test(&mut self) {
        self.next = 0;
    }
}

impl fmt::Debug for Sealer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealer")
            .field("next", &self.next)
            .field("unwritten", &(self.len - self.written))
            .finish_non_exhaustive()
    }
}

/// Opens one direction of a leg and keeps the frame boundaries: a record belongs to exactly one
/// frame, and the frame header is checked against the leg's cap as soon as it is complete.
pub struct Opener {
    state: Arc<StatelessTransportState>,
    attempt: Arc<Attempt>,
    next: u64,
    scratch: Wiped,
    pipe: Pipe,
    announced: Option<usize>,
    header: [u8; HEADER_LEN],
    have: usize,
    remaining: usize,
}

impl Opener {
    /// The record body length an envelope announces: 17 to [`MAX_RECORD`] bytes.
    ///
    /// # Errors
    /// An empty or shorter record, or an earlier failure of the attempt. A refusal retires it.
    pub fn record_len(&mut self, envelope: [u8; ENVELOPE_LEN]) -> Result<usize, SessionError> {
        self.attempt.check()?;
        let len = usize::from(u16::from_be_bytes(envelope));
        if len <= TAG_LEN {
            return self.attempt.fail(SessionError::Length { len });
        }
        self.announced = Some(len);
        Ok(len)
    }

    /// Opens the record body [`Opener::record_len`] announced. The delivery wipes its plaintext
    /// when dropped.
    ///
    /// # Errors
    /// A body of another length, a record that fails to authenticate, a frame header over the
    /// leg's cap, a record that crosses its frame's end, the record limit, or an earlier failure.
    /// Every error retires the attempt.
    pub fn open(&mut self, record: &[u8]) -> Result<Delivery<'_>, SessionError> {
        self.attempt.check()?;
        if self.announced.take() != Some(record.len()) {
            return self
                .attempt
                .fail(SessionError::Length { len: record.len() });
        }
        let n = self.decrypt(record)?;
        match self.walk(n) {
            Ok(frame_done) => Ok(Delivery {
                bytes: &mut self.scratch[..n],
                frame_done,
            }),
            Err(error) => {
                self.scratch[..n].zeroize();
                self.attempt.fail(error)
            }
        }
    }

    /// The stream's end. Clean only between frames.
    ///
    /// # Errors
    /// [`SessionError::Truncated`] inside a record or a frame, which retires the attempt.
    pub fn end(self) -> Result<(), SessionError> {
        self.attempt.check()?;
        if self.announced.is_some() || self.have > 0 {
            return self.attempt.fail(SessionError::Truncated);
        }
        Ok(())
    }

    /// Opens a confirmation or binding record whose plaintext must be exactly `expected`.
    pub(crate) fn open_exact(
        &mut self,
        record: &[u8],
        expected: &[u8],
    ) -> Result<(), SessionError> {
        self.attempt.check()?;
        if record.len() != expected.len() + TAG_LEN {
            return self
                .attempt
                .fail(SessionError::Length { len: record.len() });
        }
        let n = self.decrypt(record)?;
        let matches = self.scratch[..n] == *expected;
        self.scratch[..n].zeroize();
        if !matches {
            return self.attempt.fail(SessionError::Bind);
        }
        Ok(())
    }

    pub(crate) fn belongs_to(&self, attempt: &Arc<Attempt>) -> bool {
        Arc::ptr_eq(&self.attempt, attempt)
    }

    fn decrypt(&mut self, record: &[u8]) -> Result<usize, SessionError> {
        if self.next >= self.attempt.limit {
            return self.attempt.fail(SessionError::Exhausted);
        }
        let Ok(n) = self
            .state
            .read_message(self.next, record, &mut self.scratch)
        else {
            self.scratch.zeroize();
            return self.attempt.fail(SessionError::Authentication);
        };
        self.next += 1;
        Ok(n)
    }

    /// Places `n` opened bytes in the current frame; true when they end it.
    fn walk(&mut self, n: usize) -> Result<bool, SessionError> {
        let mut rest = &self.scratch[..n];
        if self.have < HEADER_LEN {
            let take = rest.len().min(HEADER_LEN - self.have);
            self.header[self.have..self.have + take].copy_from_slice(&rest[..take]);
            self.have += take;
            rest = &rest[take..];
            if self.have < HEADER_LEN {
                return Ok(false);
            }
            self.remaining = frame::payload_len(self.header, self.pipe)
                .map_err(|_| SessionError::FrameTooLarge)?;
        }
        if rest.len() > self.remaining {
            return Err(SessionError::CrossesFrame);
        }
        self.remaining -= rest.len();
        let done = self.remaining == 0;
        if done {
            self.have = 0;
        }
        Ok(done)
    }

    #[cfg(test)]
    pub(crate) fn next(&self) -> u64 {
        self.next
    }

    #[cfg(test)]
    pub(crate) fn reset_for_test(&mut self) {
        self.next = 0;
    }

    #[cfg(test)]
    pub(crate) fn scratch_addr(&self) -> usize {
        self.scratch.as_ptr().addr()
    }
}

impl fmt::Debug for Opener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Opener")
            .field("next", &self.next)
            .field("pipe", &self.pipe)
            .field("mid_frame", &(self.have > 0))
            .finish_non_exhaustive()
    }
}

/// One opened record's plaintext, a slice of the opener's scratch that is wiped when this drops.
pub struct Delivery<'a> {
    bytes: &'a mut [u8],
    frame_done: bool,
}

impl Delivery<'_> {
    /// The record's plaintext: the next bytes of the current frame, header included.
    pub fn bytes(&self) -> &[u8] {
        self.bytes
    }

    /// Whether this record ends its frame.
    pub fn frame_done(&self) -> bool {
        self.frame_done
    }
}

impl Drop for Delivery<'_> {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

impl fmt::Debug for Delivery<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Delivery")
            .field("len", &self.bytes.len())
            .field("frame_done", &self.frame_done)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{Opener, Sealer};
    use crate::pair::tests::{Run, envelope, established};
    use crate::{ENVELOPE_LEN, MAX_PLAINTEXT, SessionError, wipes};
    use kumochi_protocol::frame::Pipe;

    /// Seals and opens one record; returns the opened bytes and whether they end the frame.
    fn deliver(
        sealer: &mut Sealer,
        opener: &mut Opener,
        plaintext: &[u8],
    ) -> Result<(Vec<u8>, bool), SessionError> {
        sealer.seal(plaintext)?;
        let record = sealer.unwritten().to_vec();
        sealer.advance(record.len());
        let len = opener.record_len([record[0], record[1]])?;
        let delivery = opener.open(&record[ENVELOPE_LEN..ENVELOPE_LEN + len])?;
        Ok((delivery.bytes().to_vec(), delivery.frame_done()))
    }

    fn frame(len: usize) -> Vec<u8> {
        let mut frame = u32::try_from(len).expect("u32").to_le_bytes().to_vec();
        frame.extend((0..len).map(|i| u8::try_from(i % 251).expect("under 251")));
        frame
    }

    /// Sends a frame as maximally sliced records; returns each record's frame-done flag.
    fn send_frame(sealer: &mut Sealer, opener: &mut Opener, frame: &[u8]) -> Vec<bool> {
        let mut arrived = Vec::new();
        let mut done = Vec::new();
        for chunk in frame.chunks(MAX_PLAINTEXT) {
            let (bytes, frame_done) = deliver(sealer, opener, chunk).expect("delivers");
            arrived.extend(bytes);
            done.push(frame_done);
        }
        assert_eq!(arrived, frame, "the frame arrives whole");
        done
    }

    #[test]
    fn frames_go_out_as_maximally_sliced_records() {
        let Run {
            mut shell,
            mut brain,
            ..
        } = established();
        let (control, data) = (Pipe::Control.max_payload(), Pipe::Data.max_payload());
        let one = send_frame(
            &mut shell.control.sealer,
            &mut brain.control.opener,
            &frame(65_515),
        );
        assert_eq!(one, [true], "65,515 bytes fit one record");
        let two = send_frame(
            &mut shell.control.sealer,
            &mut brain.control.opener,
            &frame(65_516),
        );
        assert_eq!(two, [false, true], "65,516 need two");
        let full = send_frame(
            &mut brain.control.sealer,
            &mut shell.control.opener,
            &frame(control),
        );
        assert_eq!(full, [false, true], "a full control frame");
        let full = send_frame(&mut brain.data.sealer, &mut shell.data.opener, &frame(data));
        assert_eq!(
            full,
            [false, false, false, false, true],
            "a full data frame"
        );
        let empty = send_frame(&mut shell.data.sealer, &mut brain.data.opener, &frame(0));
        assert_eq!(
            empty,
            [true],
            "a header that declares nothing passes to the decoder"
        );
    }

    #[test]
    fn a_split_header_is_checked_against_the_cap_when_it_completes() {
        let Run {
            mut shell,
            mut brain,
            ..
        } = established();
        let (sealer, opener) = (&mut shell.control.sealer, &mut brain.control.opener);
        let at_cap = u32::try_from(Pipe::Control.max_payload())
            .expect("u32")
            .to_le_bytes();
        assert_eq!(
            deliver(sealer, opener, &at_cap[..1]),
            Ok((at_cap[..1].to_vec(), false)),
            "1"
        );
        assert_eq!(
            deliver(sealer, opener, &at_cap[1..]),
            Ok((at_cap[1..].to_vec(), false)),
            "+3"
        );

        let Run {
            mut shell,
            mut brain,
            ..
        } = established();
        let (sealer, opener) = (&mut shell.control.sealer, &mut brain.control.opener);
        let over = u32::try_from(Pipe::Control.max_payload() + 1)
            .expect("u32")
            .to_le_bytes();
        deliver(sealer, opener, &over[..1]).expect("the first header byte");
        assert_eq!(
            deliver(sealer, opener, &over[1..]),
            Err(SessionError::FrameTooLarge)
        );
        assert_eq!(
            deliver(sealer, opener, &frame(0)),
            Err(SessionError::Retired),
            "and after"
        );
    }

    #[test]
    fn a_record_that_crosses_the_end_of_its_frame_is_refused() {
        let Run {
            mut shell,
            mut brain,
            ..
        } = established();
        let mut crossing = frame(1);
        crossing.push(7);
        let refused = deliver(&mut shell.data.sealer, &mut brain.data.opener, &crossing);
        assert_eq!(refused, Err(SessionError::CrossesFrame));

        let Run {
            mut shell,
            mut brain,
            ..
        } = established();
        let (sealer, opener) = (&mut shell.data.sealer, &mut brain.data.opener);
        deliver(sealer, opener, &[0, 0]).expect("half a zero-length header");
        assert_eq!(
            deliver(sealer, opener, &[0, 0, 9]),
            Err(SessionError::CrossesFrame)
        );
    }

    #[test]
    fn empty_and_oversized_application_records_are_refused() {
        let Run {
            mut shell,
            mut brain,
            ..
        } = established();
        assert_eq!(
            shell.control.sealer.seal(&[]),
            Err(SessionError::Length { len: 16 })
        );
        let Run { mut shell, .. } = established();
        let refused = shell.control.sealer.seal(&vec![0; MAX_PLAINTEXT + 1]);
        assert_eq!(refused, Err(SessionError::Length { len: 65_536 }));
        for len in [0, 15, 16] {
            let Run { mut brain, .. } = established();
            assert_eq!(
                brain.data.opener.record_len(envelope(len)),
                Err(SessionError::Length { len })
            );
        }
        assert_eq!(
            brain.data.opener.record_len(envelope(17)),
            Ok(17),
            "one byte of plaintext"
        );
        assert_eq!(
            brain.data.opener.open(&[0; 16]).err(),
            Some(SessionError::Length { len: 16 })
        );
    }

    #[test]
    fn the_stream_may_end_only_between_frames() {
        let Run {
            mut shell,
            mut brain,
            ..
        } = established();
        send_frame(&mut shell.data.sealer, &mut brain.data.opener, &frame(3));
        assert_eq!(brain.data.opener.end(), Ok(()), "between frames");

        let Run { mut brain, .. } = established();
        brain.data.opener.record_len(envelope(20)).expect("length");
        assert_eq!(
            brain.data.opener.end(),
            Err(SessionError::Truncated),
            "inside a record"
        );

        for cut in [1, 6] {
            let Run {
                mut shell,
                mut brain,
                ..
            } = established();
            let whole = frame(5);
            deliver(
                &mut shell.data.sealer,
                &mut brain.data.opener,
                &whole[..cut],
            )
            .expect("part");
            assert_eq!(
                brain.data.opener.end(),
                Err(SessionError::Truncated),
                "after {cut} bytes"
            );
        }
    }

    #[test]
    fn a_record_is_sealed_once_and_resumed_from_its_offset() {
        let Run {
            mut shell,
            mut brain,
            ..
        } = established();
        let sealer = &mut shell.control.sealer;
        sealer.seal(&frame(4)).expect("seals");
        let record = sealer.unwritten().to_vec();
        sealer.advance(3);
        assert_eq!(
            sealer.unwritten(),
            &record[3..],
            "a short write resumes the same bytes"
        );
        assert_eq!(sealer.next(), 3, "no second nonce for the resumed record");
        sealer.advance(record.len());
        assert!(sealer.unwritten().is_empty(), "written");
        brain
            .control
            .opener
            .record_len([record[0], record[1]])
            .expect("length");
        let opened = brain
            .control
            .opener
            .open(&record[ENVELOPE_LEN..])
            .expect("opens");
        assert_eq!(opened.bytes(), frame(4), "the peer opens it");

        let Run { mut shell, .. } = established();
        shell.control.sealer.seal(&frame(1)).expect("seals");
        shell.control.sealer.advance(1);
        assert_eq!(
            shell.control.sealer.seal(&frame(1)),
            Err(SessionError::Pending)
        );
    }

    #[test]
    fn a_reflected_record_fails_to_authenticate() {
        let Run { mut shell, .. } = established();
        shell.control.sealer.seal(&frame(1)).expect("seals");
        let record = shell.control.sealer.unwritten()[ENVELOPE_LEN..].to_vec();
        shell
            .control
            .opener
            .record_len(envelope(record.len()))
            .expect("length");
        let reflected = shell.control.opener.open(&record).err();
        assert_eq!(reflected, Some(SessionError::Authentication));
    }

    #[test]
    fn a_failure_in_one_direction_retires_both_legs_and_directions() {
        let Run {
            mut shell,
            mut brain,
            ..
        } = established();
        brain.data.opener.record_len(envelope(20)).expect("length");
        assert_eq!(
            brain.data.opener.open(&[0; 20]).err(),
            Some(SessionError::Authentication)
        );
        assert_eq!(
            brain.control.sealer.seal(&frame(1)),
            Err(SessionError::Retired),
            "control out"
        );
        assert_eq!(
            brain.data.sealer.seal(&frame(1)),
            Err(SessionError::Retired),
            "data out"
        );
        shell
            .control
            .sealer
            .seal(&frame(1))
            .expect("the honest shell still seals");
        let valid = shell.control.sealer.unwritten().to_vec();
        let refused = brain.control.opener.record_len([valid[0], valid[1]]);
        assert_eq!(
            refused,
            Err(SessionError::Retired),
            "a valid record after the failure"
        );
        assert_eq!(
            brain.control.opener.open(&valid[ENVELOPE_LEN..]).err(),
            Some(SessionError::Retired)
        );
    }

    #[test]
    fn opened_plaintext_is_wiped_when_delivered_refused_or_dropped() {
        let Run {
            mut shell,
            mut brain,
            ..
        } = established();
        let (sealer, opener) = (&mut shell.data.sealer, &mut brain.data.opener);
        let sent = frame(5);
        let (arrived, _) = deliver(sealer, opener, &sent).expect("delivers");
        assert_eq!(arrived, sent, "delivered");
        assert!(
            opener.scratch[..sent.len()].iter().all(|&b| b == 0),
            "wiped after delivery"
        );

        let mut crossing = frame(1);
        crossing.extend([0xAB; 8]);
        assert_eq!(
            deliver(sealer, opener, &crossing),
            Err(SessionError::CrossesFrame)
        );
        assert!(
            opener.scratch[..crossing.len()].iter().all(|&b| b == 0),
            "wiped on refusal"
        );

        let scratch = opener.scratch_addr();
        let record = sealer.record.as_ptr().addr();
        wipes::clear();
        drop(brain);
        drop(shell);
        assert_eq!(wipes::wiped(scratch), Some(true), "opener scratch on drop");
        assert_eq!(wipes::wiped(record), Some(true), "sealer record on drop");
    }

    #[test]
    fn debug_output_carries_no_plaintext() {
        let Run {
            mut shell,
            mut brain,
            ..
        } = established();
        let marker = b"\x05\x00\x00\x00SENTI";
        shell.data.sealer.seal(marker).expect("seals");
        let record = shell.data.sealer.unwritten().to_vec();
        brain
            .data
            .opener
            .record_len([record[0], record[1]])
            .expect("length");
        let delivery = brain
            .data
            .opener
            .open(&record[ENVELOPE_LEN..])
            .expect("opens");
        let text = format!("{delivery:?}");
        drop(delivery);
        let text = format!("{text} {shell:?} {brain:?}");
        assert!(!text.contains("SENTI"), "as text: {text}");
        assert!(
            !text.contains("83, 69, 78, 84, 73"),
            "as a byte list: {text}"
        );
    }
}
