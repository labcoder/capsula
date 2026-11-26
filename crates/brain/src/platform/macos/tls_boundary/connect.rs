use std::future::{Future, poll_fn};
use std::io;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::{Context, Poll};

use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream};
use tower_layer::Layer;
use tower_service::Service;

use super::{Attempt, CONNECTING, establish};

/// A macOS TLS stream released only after its guarded handshake finishes.
pub type NativeStream = TlsStream<HandshakeIo<TcpStream>>;

pub(in crate::platform) async fn direct(
    tls: &TlsConnector,
    host: &'static str,
) -> io::Result<NativeStream> {
    let name = ServerName::try_from(host).map_err(|_| io::ErrorKind::InvalidInput)?;
    handshake(tls.clone(), name, async move {
        let tcp = TcpStream::connect((host, 443)).await?;
        tcp.set_nodelay(true)?;
        Ok(tcp)
    })
    .await
}

pub(super) async fn handshake<I>(
    tls: TlsConnector,
    name: ServerName<'static>,
    dial: impl Future<Output = io::Result<I>> + Send + 'static,
) -> io::Result<TlsStream<HandshakeIo<I>>>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    establish(async move {
        let authority = current()?;
        let mut dial = pin!(dial);
        let io = poll_fn(|cx| authority.with(|| dial.as_mut().poll(cx))?).await?;
        let mut stream = tls
            .connect(
                name,
                HandshakeIo {
                    io,
                    authority: Some(authority),
                },
            )
            .await?;
        stream.get_mut().0.authority = None;
        Ok(stream)
    })
    .await?
}

fn current() -> io::Result<Arc<Attempt>> {
    CONNECTING
        .try_with(Arc::clone)
        .map_err(|_| io::ErrorKind::PermissionDenied.into())
}

/// Transport that checks connection authority until the handshake hands it off.
pub struct HandshakeIo<I> {
    io: I,
    authority: Option<Arc<Attempt>>,
}

impl<I> std::fmt::Debug for HandshakeIo<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HandshakeIo")
    }
}

impl<I> HandshakeIo<I> {
    fn poll<T>(&mut self, run: impl FnOnce(&mut I) -> Poll<io::Result<T>>) -> Poll<io::Result<T>> {
        match &self.authority {
            Some(authority) => authority.with(|| run(&mut self.io))?,
            None => run(&mut self.io),
        }
    }
}

impl<I: AsyncRead + Unpin> AsyncRead for HandshakeIo<I> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        read: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.get_mut().poll(|io| Pin::new(io).poll_read(cx, read))
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for HandshakeIo<I> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        write: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().poll(|io| Pin::new(io).poll_write(cx, write))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll(|io| Pin::new(io).poll_flush(cx))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll(|io| Pin::new(io).poll_shutdown(cx))
    }
}

pub(in crate::platform) fn http(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    builder
        .pool_max_idle_per_host(0)
        .retry(reqwest::retry::never())
        .connector_layer(Boundary)
}

#[derive(Clone, Copy)]
struct Boundary;

impl<S> Layer<S> for Boundary {
    type Service = Connect<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Connect(inner)
    }
}

#[derive(Clone)]
struct Connect<S>(S);

type BoxError = Box<dyn std::error::Error + Send + Sync>;

impl<S, R, T> Service<R> for Connect<S>
where
    S: Service<R, Response = T, Error = BoxError> + Clone + Send + 'static,
    S::Future: Send + 'static,
    R: Send + 'static,
    T: Send + 'static,
{
    type Response = T;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<T, BoxError>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: R) -> Self::Future {
        let mut inner = self.0.clone();
        Box::pin(async move {
            establish(async move {
                poll_fn(|cx| inner.poll_ready(cx)).await?;
                current()?.with(|| ())?;
                inner.call(request).await
            })
            .await?
        })
    }
}
