//! A route-typed TLS connector whose host cannot be confused with the Responses route.

use std::fmt;
use std::io;
use std::sync::Arc;

use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream};

use crate::connections::http_client::{self, HttpError};
use crate::connections::model::conn::Connect;

pub(super) const HOST: &str = "api.anthropic.com";

fn server_name() -> io::Result<ServerName<'static>> {
    ServerName::try_from(HOST).map_err(io::Error::other)
}

/// Only connectors for the Anthropic route implement this marker.
pub trait AnthropicConnect: Connect + sealed::Sealed {}

pub(super) mod sealed {
    pub trait Sealed {}
}

/// TLS to the fixed Anthropic host, using OS roots and HTTP/1.1 without early data.
pub struct AnthropicTls(TlsConnector);

impl AnthropicTls {
    /// Construct the fixed-route connector.
    ///
    /// # Errors
    /// Refuses missing trust roots or an invalid TLS configuration.
    pub fn build() -> Result<Self, HttpError> {
        let mut config = http_client::client_config(crate::platform::trust_roots())?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self(TlsConnector::from(Arc::new(config))))
    }
}

impl fmt::Debug for AnthropicTls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AnthropicTls")
    }
}

impl Connect for AnthropicTls {
    type Io = TlsStream<TcpStream>;

    async fn connect(&self) -> io::Result<Self::Io> {
        // Tests never reach the network, so the only socket the route opens refuses in one.
        if cfg!(test) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let tcp = TcpStream::connect((HOST, 443)).await?;
        tcp.set_nodelay(true)?;
        let name = server_name()?;
        self.0.connect(name, tcp).await
    }
}

impl sealed::Sealed for AnthropicTls {}
impl AnthropicConnect for AnthropicTls {}

#[derive(Clone)]
pub(crate) enum RouteConnector {
    Native(Option<Arc<AnthropicTls>>),
    #[cfg(test)]
    Offline(Scripted),
}

pub(crate) trait RouteIo:
    tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send
{
}
impl<I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> RouteIo for I {}

impl Connect for RouteConnector {
    type Io = Box<dyn RouteIo>;

    async fn connect(&self) -> io::Result<Self::Io> {
        match self {
            Self::Native(tls) => {
                let tls = tls.as_ref().ok_or(io::ErrorKind::NotConnected)?;
                Ok(Box::new(tls.connect().await?))
            }
            #[cfg(test)]
            Self::Offline(script) => Ok(Box::new(script.connect().await?)),
        }
    }
}

impl sealed::Sealed for RouteConnector {}
impl AnthropicConnect for RouteConnector {}

/// An in-memory Anthropic peer: each connection answers with the next scripted response and
/// keeps the bytes it was sent. No socket.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct Scripted(pub(crate) Arc<std::sync::Mutex<Script>>);

#[cfg(test)]
type ScriptReader = Box<dyn tokio::io::AsyncRead + Unpin + Send>;

#[cfg(test)]
#[derive(Default)]
pub(crate) struct Script {
    pub(crate) responses: std::collections::VecDeque<Vec<u8>>,
    pub(crate) requests: Vec<Vec<u8>>,
    pub(crate) starts: usize,
    pub(crate) drops: usize,
    pub(crate) reader: Option<ScriptReader>,
    pub(crate) entered: Option<Arc<tokio::sync::Notify>>,
    pub(crate) connect_gate: Option<Arc<tokio::sync::Notify>>,
}

#[cfg(test)]
pub(crate) struct Sent(Arc<std::sync::Mutex<Script>>, usize);

#[cfg(test)]
impl Drop for Sent {
    fn drop(&mut self) {
        self.0.lock().expect("script").drops += 1;
    }
}

#[cfg(test)]
impl tokio::io::AsyncWrite for Sent {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let mut script = self.0.lock().expect("script");
        script.requests[self.1].extend_from_slice(bytes);
        std::task::Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
impl Connect for Scripted {
    type Io = tokio::io::Join<ScriptReader, Sent>;

    fn connect(&self) -> impl Future<Output = io::Result<Self::Io>> + Send {
        let mut script = self.0.lock().expect("script");
        script.starts += 1;
        if let Some(entered) = &script.entered {
            entered.notify_one();
        }
        let connect_gate = script.connect_gate.clone();
        let index = script.requests.len();
        let reader = script.reader.take().or_else(|| {
            script
                .responses
                .pop_front()
                .map(|response| -> ScriptReader { Box::new(io::Cursor::new(response)) })
        });
        let io = reader.map(|reader| tokio::io::join(reader, Sent(Arc::clone(&self.0), index)));
        if io.is_some() {
            script.requests.push(Vec::new());
        }
        async move {
            if let Some(gate) = connect_gate {
                gate.notified().await;
            }
            io.ok_or_else(|| io::ErrorKind::ConnectionRefused.into())
        }
    }
}

#[cfg(test)]
impl sealed::Sealed for Scripted {}
#[cfg(test)]
impl AnthropicConnect for Scripted {}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn fixed_route_offers_only_http11_and_the_anthropic_sni() {
        let tls = AnthropicTls::build().expect("OS trust roots");
        let config = tls.0.config();
        assert_eq!(config.alpn_protocols, [b"http/1.1".to_vec()]);
        assert!(!config.enable_early_data);
        let name = server_name().expect("fixed DNS host");
        let mut client =
            rustls::ClientConnection::new(Arc::clone(config), name).expect("TLS client");
        let mut hello = Vec::new();
        client.write_tls(&mut hello).expect("encode ClientHello");
        let mut acceptor = rustls::server::Acceptor::default();
        acceptor
            .read_tls(&mut io::Cursor::new(hello))
            .expect("read ClientHello");
        let accepted = acceptor
            .accept()
            .expect("valid ClientHello")
            .expect("complete ClientHello");
        assert_eq!(
            accepted.client_hello().server_name(),
            Some("api.anthropic.com")
        );
    }
}
