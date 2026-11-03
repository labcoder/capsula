use std::collections::HashMap;
use std::time::Duration;

use fluent_bundle::{FluentBundle, FluentResource};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use url::Url;
use zeroize::Zeroizing;

use super::{DYNAMIC_CLIENT, SignIn, SignInError};
use crate::connections::records::MAX_ID_BYTES;

/// Local policy: cap the entire HTTP header, including its terminator, at 8 KiB.
const HEADER_LIMIT: usize = 8192;
/// Local policy: one absolute read limit, not a renewed limit for each chunk.
const READ_LIMIT: Duration = Duration::from_secs(2);
/// Local policy: a reply gets at most one second, inside the overall attempt deadline.
const WRITE_LIMIT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
pub(super) enum Page {
    Received,
    Rejected,
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Event {
    Reading,
    ReadTimedOut,
    Rejected,
}

impl SignIn {
    pub(super) async fn callback(
        &self,
    ) -> Result<(Zeroizing<String>, Zeroizing<String>), SignInError> {
        loop {
            let (mut socket, peer) = self.listener.accept().await.map_err(|_| SignInError::Io)?;
            if !peer.ip().is_loopback() {
                continue;
            }
            let deadline = tokio::time::Instant::now() + READ_LIMIT;
            #[cfg(test)]
            self.event(Event::Reading);
            let read = tokio::time::timeout_at(deadline, read_header(&mut socket)).await;
            #[cfg(test)]
            if read.is_err() {
                self.event(Event::ReadTimedOut);
            }
            if tokio::time::Instant::now() >= self.deadline {
                return Err(SignInError::Expired);
            }
            let result = match read {
                Ok(Ok(header)) if tokio::time::Instant::now() < deadline => parse(
                    &header,
                    &self.redirect,
                    self.state.expose(),
                    self.saved_client.as_ref().map(|c| c.expose().as_str()),
                ),
                _ => Err(SignInError::InvalidCallback),
            };
            let accepted = !matches!(result, Err(SignInError::InvalidCallback));
            let reply = response(if accepted {
                Page::Received
            } else {
                Page::Rejected
            });
            if !matches!(
                tokio::time::timeout(WRITE_LIMIT, socket.write_all(reply.as_bytes())).await,
                Ok(Ok(()))
            ) {
                tracing::debug!("sign-in callback reply not delivered");
            }
            if accepted {
                if tokio::time::Instant::now() >= self.deadline {
                    return Err(SignInError::Expired);
                }
                return result;
            }
            #[cfg(test)]
            self.event(Event::Rejected);
        }
    }

    #[cfg(test)]
    fn event(&self, event: Event) {
        if let Some(events) = &self.events {
            events.send(event).expect("test event receiver is alive");
        }
    }
}

async fn read_header(socket: &mut TcpStream) -> Result<Zeroizing<String>, SignInError> {
    let mut bytes = Zeroizing::new([0; HEADER_LIMIT]);
    let mut length = 0;
    loop {
        let n = socket
            .read(&mut bytes[length..])
            .await
            .map_err(|_| SignInError::InvalidCallback)?;
        length += n;
        if let Some(end) = bytes[..length].windows(4).position(|v| v == b"\r\n\r\n") {
            let header =
                std::str::from_utf8(&bytes[..end + 4]).map_err(|_| SignInError::InvalidCallback)?;
            return Ok(Zeroizing::new(header.to_owned()));
        }
        if n == 0 || length == HEADER_LIMIT {
            return Err(SignInError::InvalidCallback);
        }
    }
}

pub(super) fn parse(
    request: &str,
    redirect: &str,
    state: &str,
    saved_client: Option<&str>,
) -> Result<(Zeroizing<String>, Zeroizing<String>), SignInError> {
    let invalid = SignInError::InvalidCallback;
    let mut lines = request.split("\r\n");
    let mut first = lines.next().ok_or(invalid)?.split(' ');
    if first.next() != Some("GET") {
        return Err(invalid);
    }
    let target = first.next().ok_or(invalid)?;
    if !target.starts_with("/auth/callback?")
        || target.bytes().any(|b| b <= b' ' || !b.is_ascii())
        || first.next() != Some("HTTP/1.1")
        || first.next().is_some()
    {
        return Err(invalid);
    }
    let expected = Url::parse(redirect).map_err(|_| invalid)?;
    let host = format!("127.0.0.1:{}", expected.port().ok_or(invalid)?);
    let mut hosts = 0;
    for line in lines.take_while(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or(invalid)?;
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.bytes().any(|b| b.is_ascii_control() && b != b'\t')
        {
            return Err(invalid);
        }
        if name.eq_ignore_ascii_case("host") {
            hosts += 1;
            if value.trim_matches([' ', '\t']) != host {
                return Err(invalid);
            }
        }
    }
    if hosts != 1 {
        return Err(invalid);
    }
    let url = Url::parse(&format!("http://{host}{target}")).map_err(|_| invalid)?;
    if url.fragment().is_some() || url.path() != "/auth/callback" {
        return Err(invalid);
    }
    let mut query = HashMap::new();
    for (key, value) in url.query_pairs() {
        if query
            .insert(key.into_owned(), Zeroizing::new(value.into_owned()))
            .is_some()
        {
            return Err(invalid);
        }
    }
    if !bool::from(
        query
            .get("state")
            .map_or(&[][..], |s| s.as_bytes())
            .ct_eq(state.as_bytes()),
    ) {
        return Err(invalid);
    }
    if let Some(error) = query.get("error") {
        return Err(if error.as_str() == "access_denied" {
            SignInError::Declined
        } else {
            SignInError::Authorization
        });
    }
    let code = query
        .remove("code")
        .filter(|c| !c.is_empty())
        .ok_or(invalid)?;
    let client = match (saved_client, query.remove("client_id")) {
        (Some(saved), supplied) if supplied.as_ref().is_none_or(|s| s.as_str() == saved) => {
            Zeroizing::new(saved.to_owned())
        }
        (None, Some(client)) if !client.is_empty() && client.as_str() != DYNAMIC_CLIENT => client,
        _ => return Err(invalid),
    };
    if client.len() > MAX_ID_BYTES {
        return Err(invalid);
    }
    Ok((code, client))
}

pub(super) fn response(page: Page) -> String {
    let resource =
        FluentResource::try_new(include_str!("../../../../../locales/en/brain.ftl").to_owned())
            .expect("the bundled brain catalog parses; callback tests check it");
    let language: unic_langid::LanguageIdentifier =
        "en".parse().expect("en is a valid language id");
    let mut bundle = FluentBundle::new(vec![language]);
    bundle
        .add_resource(resource)
        .expect("brain catalog has no duplicate ids");
    let (id, status) = match page {
        Page::Received => ("callback-ok", "200 OK"),
        Page::Rejected => ("callback-rejected", "400 Bad Request"),
    };
    let pattern = bundle
        .get_message(id)
        .and_then(|m| m.value())
        .expect("callback messages exist");
    let mut errors = Vec::new();
    let body = bundle.format_pattern(pattern, None, &mut errors);
    assert!(
        errors.is_empty(),
        "callback messages have no formatting errors"
    );
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\n\r\n{body}",
        body.len()
    )
}
