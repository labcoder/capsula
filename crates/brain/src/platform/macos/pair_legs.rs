//! An authenticated pair as the greeting's [`Legs`]: the shared checks in the shared order, over
//! the pair's own openers and sealers. Nothing here logs, since nothing before the commit may.

use std::io;

use kumochi_protocol::message::{AudioSession, IssueSession};
use tokio::net::unix::OwnedReadHalf;
use tokio::time::Instant;

use super::super::stream::RecordReader;
use super::{AuthenticatedPair, Opening, PairError};
use crate::ipc::{Legs, PeerAuth, SessionError};
use crate::platform::ShellProcess;
use crate::runtime::PairStart;
use crate::tap::Tap;

/// A greeting over an authenticated pair, by one deadline, from the shell that is this brain's
/// parent. Before the commit there's no tap, so the tap `greet` passes is never written.
pub(crate) struct PairLegs<'a> {
    pair: &'a mut AuthenticatedPair,
    shell: &'a ShellProcess,
    by: Instant,
}

impl<'a> PairLegs<'a> {
    /// `by` is the deadline the caller gives `greet` too.
    pub(crate) fn new(
        pair: &'a mut AuthenticatedPair,
        shell: &'a ShellProcess,
        by: Instant,
    ) -> Self {
        Self { pair, shell, by }
    }
}

fn refused(error: PairError) -> SessionError {
    match error {
        PairError::Timeout => SessionError::Timeout,
        PairError::Greeting => SessionError::NotHello,
        _ => SessionError::Protocol("greeting read out of order"),
    }
}

impl Legs for PairLegs<'_> {
    type Peer = ShellProcess;
    type Data = RecordReader<OwnedReadHalf>;

    async fn control_greeting(
        &mut self,
        _: &mut Tap,
    ) -> Result<Option<(PeerAuth<'_>, AudioSession, PairStart)>, SessionError> {
        let (bound, opening, _) = self.pair.control_greeting(self.by).await.map_err(refused)?;
        let audio = if opening == Opening::Legacy {
            AudioSession::Legacy
        } else {
            AudioSession::QueuedRequired
        };
        // A reconnect repairs a session this process served, and a parent-bound brain served none.
        Ok(Some((
            PeerAuth::SessionBound(bound),
            audio,
            PairStart::Ordinary,
        )))
    }

    async fn write_control(&mut self, frame: &[u8]) -> io::Result<()> {
        self.pair.writers[0].write_frame(frame).await
    }

    /// The pair already holds both legs.
    fn open_data(&mut self) -> impl Future<Output = Result<(), SessionError>> {
        std::future::ready(Ok(()))
    }

    async fn data_greeting(
        &mut self,
        _: &mut Tap,
    ) -> Result<Option<(PeerAuth<'_>, Option<IssueSession>)>, SessionError> {
        let (bound, echo, _) = self.pair.data_greeting(self.by).await.map_err(refused)?;
        Ok(Some((PeerAuth::SessionBound(bound), echo)))
    }

    fn bind(&mut self) -> Result<ShellProcess, SessionError> {
        Ok(self.shell.clone())
    }

    fn data(&mut self) -> &mut Self::Data {
        &mut self.pair.data
    }

    fn fence(&self) -> Result<(), SessionError> {
        self.shell.fence().map_err(|_| SessionError::ClosedEarly)
    }

    fn mismatch_notice(&self) {}
}
