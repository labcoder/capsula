//! The only destination a local connection may reach: a port on this machine's loopback
//! address. The type holds nothing else, so no host name, LAN address or URL can become one.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroU16;

use kumochi_protocol::constants::LOCAL_CHAT;
use tokio::net::TcpStream;

use super::Failure;

/// Which loopback address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Loopback {
    /// `127.0.0.1`.
    V4,
    /// `::1`.
    V6,
}

/// A nonzero port on `127.0.0.1` or `::1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopbackEndpoint {
    address: Loopback,
    port: NonZeroU16,
}

impl LoopbackEndpoint {
    /// The endpoint the user's settings name.
    ///
    /// # Errors
    /// [`Failure::Dormant`] while [`LOCAL_CHAT`] is off, then [`Failure::Endpoint`] for any text
    /// but `127.0.0.1:<port>` or `[::1]:<port>`.
    pub fn configured(text: &str) -> Result<Self, Failure> {
        if !LOCAL_CHAT {
            return Err(Failure::Dormant);
        }
        parse(text).ok_or(Failure::Endpoint)
    }

    /// The endpoint `text` names, bypassing the switch.
    #[cfg(test)]
    pub(super) fn for_tests(text: &str) -> Self {
        parse(text).expect("a loopback endpoint")
    }

    fn address(self) -> SocketAddr {
        let ip = match self.address {
            Loopback::V4 => IpAddr::V4(Ipv4Addr::LOCALHOST),
            Loopback::V6 => IpAddr::V6(Ipv6Addr::LOCALHOST),
        };
        SocketAddr::new(ip, self.port.get())
    }

    /// The request's `Host`: `127.0.0.1:<port>` or `[::1]:<port>`.
    pub(super) fn authority(self) -> String {
        self.address().to_string()
    }

    /// A TCP connection to exactly this address, with no name lookup or proxy, checked to be
    /// connected there before any byte is written.
    pub(super) async fn connect(self) -> Result<TcpStream, Failure> {
        let stream = TcpStream::connect(self.address())
            .await
            .map_err(|_| Failure::Transport)?;
        if !self.is_peer_of(&stream) {
            return Err(Failure::Transport);
        }
        Ok(stream)
    }

    fn is_peer_of(self, stream: &TcpStream) -> bool {
        stream
            .peer_addr()
            .is_ok_and(|peer| peer == self.address())
    }
}

/// `127.0.0.1:<port>` or `[::1]:<port>`, the port in plain decimal from 1 to 65535.
fn parse(text: &str) -> Option<LoopbackEndpoint> {
    let (address, port) = if let Some(port) = text.strip_prefix("127.0.0.1:") {
        (Loopback::V4, port)
    } else {
        (Loopback::V6, text.strip_prefix("[::1]:")?)
    };
    if port.len() > 5 || port.starts_with('0') || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let port = NonZeroU16::new(port.parse().ok()?)?;
    Some(LoopbackEndpoint { address, port })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_two_loopback_literals_and_a_nonzero_port_parse() {
        for (text, authority) in [
            ("127.0.0.1:1", "127.0.0.1:1"),
            ("127.0.0.1:8080", "127.0.0.1:8080"),
            ("127.0.0.1:65535", "127.0.0.1:65535"),
            ("[::1]:1", "[::1]:1"),
            ("[::1]:11434", "[::1]:11434"),
            ("[::1]:65535", "[::1]:65535"),
        ] {
            assert_eq!(parse(text).map(LoopbackEndpoint::authority).as_deref(), Some(authority));
        }
        for text in [
            "",
            "127.0.0.1",
            "127.0.0.1:",
            "127.0.0.1:0",
            "127.0.0.1:65536",
            "127.0.0.1:99999",
            "127.0.0.1:08080",
            "127.0.0.1:+80",
            "127.0.0.1:80 ",
            " 127.0.0.1:80",
            "127.0.0.1:80\r\nX: y",
            "127.0.0.1:8080/v1",
            "127.0.0.1:8080?a=b",
            "http://127.0.0.1:8080",
            "user@127.0.0.1:8080",
            "localhost:8080",
            "127.1:8080",
            "0177.0.0.1:8080",
            "0x7f.0.0.1:8080",
            "127.0.0.2:8080",
            "0.0.0.0:8080",
            "192.168.1.2:8080",
            "[::]:8080",
            "[::1]",
            "::1:8080",
            "[0:0:0:0:0:0:0:1]:8080",
            "[::ffff:127.0.0.1]:8080",
            "[::1%1]:8080",
            "[::1]:0",
        ] {
            assert_eq!(parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn configured_refuses_every_endpoint_while_switched_off() {
        for text in ["127.0.0.1:8080", "[::1]:8080", "localhost:8080"] {
            assert_eq!(LoopbackEndpoint::configured(text), Err(Failure::Dormant));
        }
    }

    #[tokio::test]
    async fn a_socket_connected_elsewhere_is_not_the_endpoint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("address").port();
        let stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let here = LoopbackEndpoint::for_tests(&format!("127.0.0.1:{port}"));
        let other = port.checked_add(1).unwrap_or(1);
        let elsewhere = LoopbackEndpoint::for_tests(&format!("127.0.0.1:{other}"));
        assert!(here.is_peer_of(&stream));
        assert!(!elsewhere.is_peer_of(&stream));
    }
}
