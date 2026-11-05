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
pub trait AnthropicConnect: Connect {}

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
        let tcp = TcpStream::connect((HOST, 443)).await?;
        tcp.set_nodelay(true)?;
        let name = server_name()?;
        self.0.connect(name, tcp).await
    }
}

impl AnthropicConnect for AnthropicTls {}

#[cfg(test)]
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
