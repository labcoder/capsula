//! One revocable credential's boundary for connect and I/O polls.

use std::future::{Future, poll_fn};
use std::io;
use std::pin::{Pin, pin};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Notify;

use super::tls::{AnthropicConnect, sealed};
use crate::connections::model::conn::Connect;

#[derive(Clone, Debug, Default)]
pub(crate) struct Fence(Arc<Cut>);

#[derive(Debug, Default)]
struct Cut {
    closed: Mutex<bool>,
    changed: Notify,
}

impl Fence {
    pub(crate) fn revoke(&self) {
        *self.0.closed.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.0.changed.notify_waiters();
    }

    pub(crate) fn open(&self) -> bool {
        self.with(|| ()).is_ok()
    }

    pub(crate) fn with<T>(&self, poll: impl FnOnce() -> T) -> io::Result<T> {
        let closed = self
            .0
            .closed
            .lock()
            .map_err(|_| io::ErrorKind::Interrupted)?;
        if *closed {
            return Err(io::ErrorKind::Interrupted.into());
        }
        Ok(poll())
    }

    async fn until_revoked(&self) {
        let changed = self.0.changed.notified();
        if self.open() {
            changed.await;
        }
    }

    pub(crate) async fn while_open<T>(&self, work: impl Future<Output = T>) -> Option<T> {
        tokio::select! {
            biased;
            () = self.until_revoked() => None,
            value = work => Some(value),
        }
    }
}

pub(crate) struct Fenced<C>(pub(crate) C, pub(crate) Fence);

impl<C: AnthropicConnect> Connect for Fenced<C> {
    type Io = Fenced<C::Io>;

    async fn connect(&self) -> io::Result<Self::Io> {
        let mut opening = pin!(self.1.with(|| self.0.connect())?);
        let io = poll_fn(|cx| self.1.with(|| opening.as_mut().poll(cx))?).await?;
        Ok(Fenced(io, self.1.clone()))
    }
}

impl<C: AnthropicConnect> sealed::Sealed for Fenced<C> {}
impl<C: AnthropicConnect> AnthropicConnect for Fenced<C> {}

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

#[cfg(test)]
mod tests;
