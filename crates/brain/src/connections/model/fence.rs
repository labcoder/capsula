//! One revocable authority's boundary for connect and I/O polls: once revoked, no poll it guards
//! runs again, and a poll already running finishes first.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::platform::ConnectCut;

#[derive(Clone, Debug, Default)]
pub(crate) struct Fence(Arc<ConnectCut>);

impl Fence {
    pub(crate) fn same_cut(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub(crate) fn cut(&self) -> &Arc<ConnectCut> {
        &self.0
    }

    pub(crate) fn revoke(&self) {
        self.0.close();
    }

    pub(crate) fn open(&self) -> bool {
        self.with(|| ()).is_ok()
    }

    pub(crate) fn with<T>(&self, poll: impl FnOnce() -> T) -> io::Result<T> {
        self.0.with(poll)
    }

    pub(crate) async fn while_open<T>(&self, work: impl Future<Output = T>) -> Option<T> {
        tokio::select! {
            biased;
            () = self.0.until_closed() => None,
            value = work => Some(value),
        }
    }
}

/// An I/O whose every poll runs inside its fence.
pub(crate) struct Fenced<C>(pub(crate) C, pub(crate) Fence);

impl<Io: AsyncRead + Unpin> AsyncRead for Fenced<Io> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let Self(io, fence) = self.get_mut();
        fence.with(|| Pin::new(io).poll_read(cx, buf))?
    }
}

impl<Io: AsyncWrite + Unpin> AsyncWrite for Fenced<Io> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let Self(io, fence) = self.get_mut();
        fence.with(|| Pin::new(io).poll_write(cx, buf))?
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Self(io, fence) = self.get_mut();
        fence.with(|| Pin::new(io).poll_flush(cx))?
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Self(io, fence) = self.get_mut();
        fence.with(|| Pin::new(io).poll_shutdown(cx))?
    }
}
