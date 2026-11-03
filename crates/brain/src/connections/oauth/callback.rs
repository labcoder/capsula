use std::collections::HashMap;
use std::io::ErrorKind;
use std::sync::LazyLock;
use std::time::Duration;

use fluent_bundle::{FluentBundle, FluentResource};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use url::form_urlencoded;
use zeroize::Zeroizing;

use super::{DYNAMIC_CLIENT, SignIn, SignInError};
use crate::connections::records::MAX_ID_BYTES;

/// Local policy: cap the entire HTTP header, including its terminator, at 32 KiB.
const HEADER_LIMIT: usize = 32768;
/// Local policy: one absolute read limit, not a renewed limit for each chunk.
const READ_LIMIT: Duration = Duration::from_secs(2);
/// Local policy: a reply gets at most one second, inside the overall attempt deadline.
const WRITE_LIMIT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
pub(super) enum Page {
    Received,
    Rejected,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CallbackError {
    Rejected,
    Terminal(SignInError),
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Event {
    Reading,
    Rejected,
}

impl SignIn {
    pub(super) async fn callback(
        &self,
    ) -> Result<(Zeroizing<String>, Zeroizing<String>), SignInError> {
        loop {
            let (mut socket, _) = match self.listener.accept().await {
                Ok(connection) => connection,
                Err(error) if retry_accept(error.kind()) => {
                    continue;
                }
                Err(_) => return Err(SignInError::Io),
            };
            #[cfg(test)]
            self.event(Event::Reading);
            let read = read_header(&mut socket).await;
            if tokio::time::Instant::now() >= self.deadline {
                return Err(SignInError::Expired);
            }
            let result = read.and_then(|header| {
                parse(
                    &header,
                    &self.redirect,
                    self.state.expose(),
                    self.saved_client.as_ref().map(|c| c.expose().as_str()),
                )
            });
            let page = match &result {
                Ok(_)
                | Err(CallbackError::Terminal(
                    SignInError::Declined | SignInError::Authorization,
                )) => Page::Received,
                Err(CallbackError::Terminal(_)) => Page::Failed,
                Err(CallbackError::Rejected) => Page::Rejected,
            };
            let reply = response(page);
            if !matches!(
                tokio::time::timeout(WRITE_LIMIT, socket.write_all(reply.as_bytes())).await,
                Ok(Ok(()))
            ) {
                tracing::debug!("sign-in callback reply not delivered");
            }
            if tokio::time::Instant::now() >= self.deadline {
                return Err(SignInError::Expired);
            }
            match result {
                Ok(result) => return Ok(result),
                Err(CallbackError::Terminal(error)) => return Err(error),
                Err(CallbackError::Rejected) => {}
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

pub(super) async fn read_header(
    socket: &mut (impl AsyncRead + Unpin),
) -> Result<Zeroizing<String>, CallbackError> {
    let deadline = tokio::time::Instant::now() + READ_LIMIT;
    let header = tokio::time::timeout_at(deadline, async {
        let mut bytes = Zeroizing::new(vec![0; HEADER_LIMIT + 1]);
        let mut length = 0;
        loop {
            let n = socket
                .read(&mut bytes[length..])
                .await
                .map_err(|_| CallbackError::Rejected)?;
            length += n;
            if let Some(end) = bytes[..length].windows(4).position(|v| v == b"\r\n\r\n") {
                if end + 4 > HEADER_LIMIT {
                    return Err(CallbackError::Rejected);
                }
                let header =
                    std::str::from_utf8(&bytes[..end + 4]).map_err(|_| CallbackError::Rejected)?;
                return Ok(Zeroizing::new(header.to_owned()));
            }
            if n == 0 || length > HEADER_LIMIT {
                return Err(CallbackError::Rejected);
            }
        }
    })
    .await
    .map_err(|_| CallbackError::Rejected)??;
    if tokio::time::Instant::now() >= deadline {
        return Err(CallbackError::Rejected);
    }
    Ok(header)
}

pub(super) fn parse(
    request: &str,
    redirect: &str,
    state: &str,
    saved_client: Option<&str>,
) -> Result<(Zeroizing<String>, Zeroizing<String>), CallbackError> {
    let invalid = CallbackError::Rejected;
    let mut lines = request.split("\r\n");
    let mut first = lines.next().ok_or(invalid)?.split(' ');
    if first.next() != Some("GET") {
        return Err(invalid);
    }
    let target = first.next().ok_or(invalid)?;
    let query = target.strip_prefix("/auth/callback?").ok_or(invalid)?;
    if target.contains('#')
        || target.bytes().any(|b| b <= b' ' || !b.is_ascii())
        || first.next() != Some("HTTP/1.1")
        || first.next().is_some()
    {
        return Err(invalid);
    }
    let host = redirect
        .strip_prefix("http://")
        .and_then(|r| r.strip_suffix("/auth/callback"))
        .ok_or(invalid)?;
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
    let mut parameters = HashMap::new();
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        if parameters
            .insert(key.into_owned(), Zeroizing::new(value.into_owned()))
            .is_some()
        {
            return Err(invalid);
        }
    }
    if !bool::from(
        parameters
            .get("state")
            .map_or(&[][..], |s| s.as_bytes())
            .ct_eq(state.as_bytes()),
    ) {
        return Err(invalid);
    }
    if let Some(error) = parameters.get("error") {
        return Err(CallbackError::Terminal(
            if error.as_str() == "access_denied" {
                SignInError::Declined
            } else {
                SignInError::Authorization
            },
        ));
    }
    let failed = CallbackError::Terminal(SignInError::SignInFailed);
    let code = parameters
        .remove("code")
        .filter(|c| !c.is_empty())
        .ok_or(failed)?;
    let client = match (saved_client, parameters.remove("client_id")) {
        (Some(saved), supplied) if supplied.as_ref().is_none_or(|s| s.as_str() == saved) => {
            Zeroizing::new(saved.to_owned())
        }
        (None, Some(client)) if !client.is_empty() && client.as_str() != DYNAMIC_CLIENT => client,
        _ => return Err(failed),
    };
    if client.len() > MAX_ID_BYTES {
        return Err(failed);
    }
    Ok((code, client))
}

pub(super) fn retry_accept(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::Interrupted
    )
}

static PAGES: LazyLock<Option<[String; 3]>> = LazyLock::new(render_pages);

pub(super) fn response(page: Page) -> &'static str {
    let Some(pages) = PAGES.as_ref() else {
        return "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    };
    &pages[match page {
        Page::Received => 0,
        Page::Rejected => 1,
        Page::Failed => 2,
    }]
}

fn render_pages() -> Option<[String; 3]> {
    let resource =
        FluentResource::try_new(include_str!("../../../../../locales/en/brain.ftl").to_owned())
            .ok()?;
    let language: unic_langid::LanguageIdentifier = "en".parse().ok()?;
    let mut bundle = FluentBundle::new(vec![language]);
    bundle.add_resource(resource).ok()?;
    let mut pages = [String::new(), String::new(), String::new()];
    for ((id, status), page) in [
        ("callback-ok", "200 OK"),
        ("callback-rejected", "400 Bad Request"),
        ("callback-failed", "400 Bad Request"),
    ]
    .into_iter()
    .zip(&mut pages)
    {
        let pattern = bundle.get_message(id).and_then(|m| m.value())?;
        let mut errors = Vec::new();
        let body = bundle.format_pattern(pattern, None, &mut errors);
        if !errors.is_empty() {
            return None;
        }
        *page = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\n\r\n{body}",
            body.len()
        );
    }
    Some(pages)
}
