use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::time::timeout;

use super::*;

#[derive(Default)]
struct Probe {
    starts: AtomicUsize,
    polls: AtomicUsize,
    bytes: AtomicUsize,
    flushes: AtomicUsize,
}

struct Peer {
    probe: Arc<Probe>,
    connect_waits: bool,
    accepts: usize,
}

impl sealed::Sealed for Peer {}
impl AnthropicConnect for Peer {}

impl Connect for Peer {
    type Io = Peer;

    fn connect(&self) -> impl Future<Output = io::Result<Self::Io>> + Send {
        self.probe.starts.fetch_add(1, SeqCst);
        poll_fn(|_| {
            self.probe.polls.fetch_add(1, SeqCst);
            if self.connect_waits {
                return Poll::Pending;
            }
            Poll::Ready(Ok(Self {
                probe: Arc::clone(&self.probe),
                connect_waits: false,
                accepts: self.accepts,
            }))
        })
    }
}

impl AsyncRead for Peer {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}

impl AsyncWrite for Peer {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let accepted = self.accepts.min(bytes.len());
        if accepted == 0 {
            return Poll::Pending;
        }
        self.accepts -= accepted;
        self.probe.bytes.fetch_add(accepted, SeqCst);
        Poll::Ready(Ok(accepted))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.probe.flushes.fetch_add(1, SeqCst);
        Poll::Pending
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn poll_once<T>(future: Pin<&mut impl Future<Output = T>>) -> Poll<T> {
    future.poll(&mut Context::from_waker(std::task::Waker::noop()))
}

#[tokio::test]
async fn revocation_stops_queued_and_paused_connects() {
    let fence = Fence::default();
    let probe = Arc::new(Probe::default());
    let client = Fenced(
        Peer {
            probe: Arc::clone(&probe),
            connect_waits: true,
            accepts: 0,
        },
        fence.clone(),
    );
    let mut paused = pin!(client.connect());
    let mut queued = pin!(client.connect());
    assert!(poll_once(paused.as_mut()).is_pending());
    assert_eq!(probe.starts.load(SeqCst), 1);
    assert_eq!(probe.polls.load(SeqCst), 1);
    fence.revoke();
    for opening in [paused, queued.as_mut()] {
        assert_eq!(
            opening.await.err().expect("revoked").kind(),
            io::ErrorKind::Interrupted
        );
    }
    assert_eq!(probe.starts.load(SeqCst), 1);
    assert_eq!(probe.polls.load(SeqCst), 1);
    assert_eq!(probe.bytes.load(SeqCst), 0);
}

#[tokio::test]
async fn revocation_wakes_pending_uploads_and_refuses_further_writes_or_flushes() {
    for accepted in [0, 2, 4] {
        let fence = Fence::default();
        let probe = Arc::new(Probe::default());
        let client = Fenced(
            Peer {
                probe: Arc::clone(&probe),
                connect_waits: false,
                accepts: accepted,
            },
            fence.clone(),
        );
        let mut io = client.connect().await.expect("offline peer");
        {
            let mut attempt = pin!(fence.while_open(async {
                io.write_all(b"body").await?;
                io.flush().await
            }));
            assert!(poll_once(attempt.as_mut()).is_pending());
            assert_eq!(probe.bytes.load(SeqCst), accepted);
            assert_eq!(probe.flushes.load(SeqCst), usize::from(accepted == 4));
            fence.revoke();
            let stopped = timeout(Duration::from_secs(1), attempt)
                .await
                .expect("woken by revocation");
            assert!(stopped.is_none());
        }
        assert_eq!(
            io.write(b"late").await.expect_err("closed").kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(
            io.flush().await.expect_err("closed").kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(probe.bytes.load(SeqCst), accepted);
        assert_eq!(probe.flushes.load(SeqCst), usize::from(accepted == 4));
    }
}
