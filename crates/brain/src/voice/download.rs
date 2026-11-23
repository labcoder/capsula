//! Bounded, resumable model downloads against the pinned voice manifest.

#[cfg(test)]
mod tests;

use std::fs::File;
use std::future::Future;
use std::io::{Read, Seek, Write};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use kumochi_protocol::constants;
use kumochi_protocol::voice::{ModelFile, VoicePart};
use reqwest::header::{ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use rustls::pki_types::CertificateDer;
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio::time::{self, Instant};
use url::{Host, Url};

use crate::platform::{
    Untrusted, check_model_final, create_model_dir, create_model_write, discard_model,
    hold_model_creation, model_final, open_model_write, publish_model,
};

const IDLE: Duration = Duration::from_secs(30);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

pub(crate) struct Downloader {
    client: reqwest::Client,
    models: PathBuf,
    rules: Rules,
    #[cfg(test)]
    base: Option<Url>,
    #[cfg(test)]
    space: Option<u64>,
}

/// What a fetch is doing now; a change is sent at once, unlike byte counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FetchPhase {
    /// Hashing bytes already on disk.
    Verifying,
    /// Requesting or receiving bytes.
    Downloading,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Progress {
    pub phase: FetchPhase,
    pub done: u64,
    pub total: u64,
}

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DownloadError {
    #[error("model downloads are unsupported on this platform")]
    UnsupportedPlatform,
    #[error("insufficient model space")]
    NoSpace { needed: u64 },
    #[error("model request refused")]
    Refused,
    #[error("model size mismatch")]
    Size,
    #[error("model hash mismatch")]
    Hash,
    #[error("model download timed out")]
    Timeout,
    #[error("model transport failed")]
    Transport,
    #[error("model storage failed")]
    Io,
    #[error("model download cancelled")]
    Cancelled,
    #[error("the speech model path is unsafe")]
    ModelPath,
}

/// A part's fetch, as the installer starts it.
pub(crate) trait Fetch: Send + Sync + std::fmt::Debug {
    fn fetch<'a>(
        &'a self,
        part: VoicePart,
        progress: &'a watch::Sender<Progress>,
        cancel: &'a watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<(), DownloadError>> + Send + 'a>>;
}

/// Fetches into its models folder with a downloader made for each fetch, so start-up builds none.
pub(crate) struct Online(pub(crate) PathBuf);

impl std::fmt::Debug for Online {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Online").finish_non_exhaustive()
    }
}

impl Fetch for Online {
    fn fetch<'a>(
        &'a self,
        part: VoicePart,
        progress: &'a watch::Sender<Progress>,
        cancel: &'a watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<(), DownloadError>> + Send + 'a>> {
        Box::pin(async move {
            let downloader = Downloader::new(self.0.clone())?;
            downloader.fetch(part, progress, cancel).await
        })
    }
}

#[derive(Clone, Default)]
enum Rules {
    #[default]
    Production,
    #[cfg(test)]
    Fixture { port: u16, hosts: Vec<String> },
}

impl Rules {
    fn accepts(&self, url: &Url) -> bool {
        if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
            return false;
        }
        match self {
            Self::Production => {
                url.port_or_known_default() == Some(443)
                    && matches!(url.host(), Some(Host::Domain(host))
                    if host == "huggingface.co" || host.ends_with(".hf.co"))
            }
            #[cfg(test)]
            Self::Fixture { port, hosts } => {
                url.port_or_known_default() == Some(*port)
                    && url
                        .host_str()
                        .is_some_and(|host| hosts.iter().any(|h| h == host))
            }
        }
    }
}

impl Downloader {
    pub(crate) fn new(models: PathBuf) -> Result<Self, DownloadError> {
        let roots = crate::platform::trust_roots().map_err(|error| match error {
            crate::platform::TrustRootsError::Unsupported => DownloadError::UnsupportedPlatform,
            _ => DownloadError::Transport,
        })?;
        Self::from_roots(models, roots.certificates, Rules::default())
    }

    fn from_roots(
        models: PathBuf,
        roots: Vec<Vec<u8>>,
        rules: Rules,
    ) -> Result<Self, DownloadError> {
        let mut store = rustls::RootCertStore::empty();
        let (usable, _) =
            store.add_parsable_certificates(roots.into_iter().map(CertificateDer::from));
        if usable == 0 {
            return Err(DownloadError::Transport);
        }
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| DownloadError::Transport)?
        .with_root_certificates(store)
        .with_no_client_auth();
        let redirects = rules.clone();
        let client = reqwest::Client::builder()
            .use_preconfigured_tls(tls)
            .https_only(true)
            .no_proxy()
            .referer(false)
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .user_agent(format!(
                "{}/{}",
                constants::DISPLAY_NAME,
                env!("CARGO_PKG_VERSION")
            ))
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() > 5 || !redirects.accepts(attempt.url()) {
                    attempt.error("model redirect refused")
                } else {
                    attempt.follow()
                }
            }))
            .connect_timeout(IDLE)
            .build()
            .map_err(|_| DownloadError::Transport)?;
        Ok(Self {
            client,
            models,
            rules,
            #[cfg(test)]
            base: None,
            #[cfg(test)]
            space: None,
        })
    }

    /// The caller supplies the models folder under `platform::data_dir`; no network value
    /// contributes to any local path. The installer's one fetch is its only caller.
    pub(crate) async fn fetch(
        &self,
        part: VoicePart,
        progress: &watch::Sender<Progress>,
        cancel: &watch::Receiver<bool>,
    ) -> Result<(), DownloadError> {
        self.fetch_files(&part.files().collect::<Vec<_>>(), progress, cancel)
            .await
    }

    async fn fetch_files(
        &self,
        files: &[&ModelFile],
        progress: &watch::Sender<Progress>,
        cancel: &watch::Receiver<bool>,
    ) -> Result<(), DownloadError> {
        let mut report = Reporter {
            sender: progress,
            last: Instant::now(),
            value: Progress {
                phase: FetchPhase::Verifying,
                done: 0,
                total: files.iter().map(|f| f.bytes).sum(),
            },
        };
        report.sender.send_replace(report.value);
        let Some(first) = files.first() else {
            return Ok(());
        };
        if files.iter().any(|model| model.part != first.part) {
            return Err(DownloadError::Refused);
        }
        let dir = self.models.join(first.part.dir());
        let mut held = create_model_dir(&self.models).map_err(path_error)?;
        let root_creation = hold_model_creation(&self.models).map_err(path_error)?;
        #[cfg(test)]
        crate::platform::paused(&self.models);
        held.extend(create_model_dir(&dir).map_err(path_error)?);
        let part_creation = hold_model_creation(&dir).map_err(path_error)?;
        #[cfg(test)]
        crate::platform::paused(&dir);
        let mut checked = Vec::new();
        for &model in files {
            check_cancel(cancel)?;
            let path = dir.join(model.name);
            let partial = dir.join(format!("{}.part", model.name));
            let final_file = optional_file(model_final(&path))?;
            let existing = optional_file(open_model_write(&partial))?;
            checked.push((model, path, partial, final_file, existing));
        }
        let mut pending = Vec::new();
        let mut done = 0;
        let mut missing = 0;
        for (model, path, partial, mut final_file, existing) in checked {
            check_cancel(cancel)?;
            if let Some(mut file) = final_file.take() {
                if file.metadata().map_err(|_| DownloadError::Io)?.len() == model.bytes
                    && hash_matches(hash_file(&mut file, cancel).await?.0, model.sha256)
                {
                    done += model.bytes;
                    continue;
                }
                #[cfg(test)]
                crate::platform::paused(&partial);
                check_model_final(&file, &path).map_err(path_error)?;
                final_file = Some(file);
            }
            let length = if let Some(file) = &existing {
                file.metadata().map_err(|_| DownloadError::Io)?.len()
            } else {
                0
            };
            missing += model.bytes - if length <= model.bytes { length } else { 0 };
            pending.push((model, path, partial, final_file, existing));
        }
        self.check_space(missing)?;
        let mut ready = Vec::new();
        for (model, path, partial, final_file, existing) in pending {
            check_cancel(cancel)?;
            let file = match existing {
                Some(file) => file,
                None => create_model_write(&partial).map_err(path_error)?,
            };
            ready.push((model, path, final_file, file));
        }
        drop((root_creation, part_creation));
        report.value.done = done;
        report.sender.send_replace(report.value);
        for (model, path, final_file, mut file) in ready {
            if let Some(file) = final_file {
                #[cfg(test)]
                crate::platform::paused(&path);
                discard_model(file).map_err(|_| DownloadError::Io)?;
            }
            // `fetch_file` hashes the bytes already in the `.part` first.
            report.phase(FetchPhase::Verifying);
            let result = self.fetch_file(model, &mut file, &mut report, cancel).await;
            if let Err(error) = result {
                if matches!(error, DownloadError::Size | DownloadError::Hash) {
                    discard_model(file).map_err(|_| DownloadError::Io)?;
                }
                tracing::warn!(?error, "voice download stopped");
                return Err(error);
            }
            publish_model(
                file,
                held.last().expect("the model directory is held"),
                &path,
            )
            .map_err(|_| DownloadError::Io)?;
            done += model.bytes;
            report.value.done = done;
        }
        report.finish().await;
        Ok(())
    }

    fn check_space(&self, missing: u64) -> Result<(), DownloadError> {
        let needed = missing + missing.div_ceil(10);
        let available = {
            #[cfg(test)]
            if let Some(space) = self.space {
                space
            } else {
                crate::platform::free_space(&self.models).map_err(|_| DownloadError::Io)?
            }
            #[cfg(not(test))]
            crate::platform::free_space(&self.models).map_err(|_| DownloadError::Io)?
        };
        if available < needed {
            return Err(DownloadError::NoSpace { needed });
        }
        Ok(())
    }

    async fn fetch_file(
        &self,
        model: &ModelFile,
        file: &mut File,
        report: &mut Reporter<'_>,
        cancel: &watch::Receiver<bool>,
    ) -> Result<(), DownloadError> {
        if file.metadata().map_err(|_| DownloadError::Io)?.len() > model.bytes {
            file.set_len(0).map_err(|_| DownloadError::Io)?;
        }
        let (mut hash, mut offset) = hash_file(file, cancel).await?;
        if offset == model.bytes {
            if !hash_matches(hash, model.sha256) {
                return Err(DownloadError::Hash);
            }
            return file.sync_all().map_err(|_| DownloadError::Io);
        }
        let mut restarted = false;
        let base_done = report.value.done;
        loop {
            let url = Url::parse(model.url).map_err(|_| DownloadError::Refused)?;
            #[cfg(test)]
            let url = if let Some(base) = &self.base {
                base.join(model.name).map_err(|_| DownloadError::Refused)?
            } else {
                url
            };
            if !self.rules.accepts(&url) {
                return Err(DownloadError::Refused);
            }
            report.phase(FetchPhase::Downloading);
            let mut request = self.client.get(url).header(ACCEPT_ENCODING, "identity");
            if offset != 0 {
                request = request.header(RANGE, format!("bytes={offset}-"));
            }
            let mut response = bounded(request.send(), cancel, Instant::now() + IDLE)
                .await?
                .map_err(|error| transport_error(&error))?;
            let status = response.status().as_u16();
            if status == 416
                || (status == 206 && !valid_range(response.headers(), offset, model.bytes))
            {
                if restarted {
                    return Err(DownloadError::Refused);
                }
                file.set_len(0).map_err(|_| DownloadError::Io)?;
                file.rewind().map_err(|_| DownloadError::Io)?;
                offset = 0;
                hash = Sha256::new();
                restarted = true;
                continue;
            }
            if status == 429 || (500..600).contains(&status) {
                return Err(DownloadError::Transport);
            }
            if status != 200 && (status != 206 || offset == 0) {
                return Err(DownloadError::Refused);
            }
            if status == 200 {
                if !valid_length(response.headers(), model.bytes) {
                    return Err(DownloadError::Size);
                }
                file.set_len(0).map_err(|_| DownloadError::Io)?;
                file.rewind().map_err(|_| DownloadError::Io)?;
                offset = 0;
                hash = Sha256::new();
            }
            let mut deadline = Instant::now() + IDLE;
            loop {
                let chunk = bounded(response.chunk(), cancel, deadline)
                    .await?
                    .map_err(|error| transport_error(&error))?;
                let Some(chunk) = chunk else {
                    break;
                };
                if chunk.is_empty() {
                    continue;
                }
                deadline = Instant::now() + IDLE;
                let count = u64::try_from(chunk.len()).map_err(|_| DownloadError::Size)?;
                if count > model.bytes - offset {
                    return Err(DownloadError::Size);
                }
                file.write_all(&chunk).map_err(|_| DownloadError::Io)?;
                hash.update(&chunk);
                offset += count;
                report.update(base_done + offset);
            }
            if offset != model.bytes {
                return Err(DownloadError::Transport);
            }
            if !hash_matches(hash, model.sha256) {
                return Err(DownloadError::Hash);
            }
            file.flush().map_err(|_| DownloadError::Io)?;
            return file.sync_all().map_err(|_| DownloadError::Io);
        }
    }
}

fn optional_file(opened: Result<File, Untrusted>) -> Result<Option<File>, DownloadError> {
    match opened {
        Ok(file) => Ok(Some(file)),
        Err(Untrusted::Missing) => Ok(None),
        Err(error) => Err(path_error(error)),
    }
}

fn path_error(error: Untrusted) -> DownloadError {
    match error {
        Untrusted::Reparse | Untrusted::Unsafe => DownloadError::ModelPath,
        Untrusted::Missing | Untrusted::Io => DownloadError::Io,
    }
}

async fn hash_file(
    file: &mut File,
    cancel: &watch::Receiver<bool>,
) -> Result<(Sha256, u64), DownloadError> {
    file.rewind().map_err(|_| DownloadError::Io)?;
    let mut hash = Sha256::new();
    let mut length = 0;
    let mut buffer = vec![0; 64 * 1024];
    loop {
        check_cancel(cancel)?;
        let count = file.read(&mut buffer).map_err(|_| DownloadError::Io)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        length += u64::try_from(count).map_err(|_| DownloadError::Io)?;
        tokio::task::yield_now().await;
    }
    Ok((hash, length))
}

fn hash_matches(hash: Sha256, expected: &str) -> bool {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    expected.len() == 64
        && hash
            .finalize()
            .iter()
            .zip(expected.as_bytes().as_chunks::<2>().0)
            .all(|(&byte, pair)| {
                *pair == [HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 15)]]
            })
}

fn valid_range(headers: &reqwest::header::HeaderMap, offset: u64, size: u64) -> bool {
    offset > 0
        && size > offset
        && headers.get_all(CONTENT_RANGE).iter().count() == 1
        && headers.get(CONTENT_RANGE).is_some_and(|value| {
            value.as_bytes() == format!("bytes {offset}-{}/{size}", size - 1).as_bytes()
        })
}

fn valid_length(headers: &reqwest::header::HeaderMap, size: u64) -> bool {
    let values = headers.get_all(CONTENT_LENGTH);
    if values.iter().count() > 1 {
        return false;
    }
    values.iter().next().is_none_or(|value| {
        value.to_str().is_ok_and(|text| {
            !text.is_empty()
                && text.bytes().all(|b| b.is_ascii_digit())
                && text.parse::<u64>() == Ok(size)
        })
    })
}

fn check_cancel(cancel: &watch::Receiver<bool>) -> Result<(), DownloadError> {
    if *cancel.borrow() {
        Err(DownloadError::Cancelled)
    } else {
        Ok(())
    }
}

async fn bounded<T>(
    future: impl Future<Output = T>,
    cancel: &watch::Receiver<bool>,
    deadline: Instant,
) -> Result<T, DownloadError> {
    let mut cancel = cancel.clone();
    tokio::select! {
        biased;
        () = async {
            if cancel.wait_for(|value| *value).await.is_err() { std::future::pending::<()>().await; }
        } => Err(DownloadError::Cancelled),
        () = time::sleep_until(deadline) => Err(DownloadError::Timeout),
        value = future => Ok(value),
    }
}

fn transport_error(error: &reqwest::Error) -> DownloadError {
    if error.is_redirect() {
        DownloadError::Refused
    } else if error.is_timeout() {
        DownloadError::Timeout
    } else {
        DownloadError::Transport
    }
}

struct Reporter<'a> {
    sender: &'a watch::Sender<Progress>,
    last: Instant,
    value: Progress,
}

impl Reporter<'_> {
    /// Sent at once when it changes; only byte counts wait for the interval.
    fn phase(&mut self, phase: FetchPhase) {
        if self.value.phase != phase {
            self.value.phase = phase;
            self.sender.send_replace(self.value);
            self.last = Instant::now();
        }
    }

    fn update(&mut self, done: u64) {
        self.value.done = done;
        if self.last.elapsed() >= PROGRESS_INTERVAL {
            self.sender.send_replace(self.value);
            self.last = Instant::now();
        }
    }

    async fn finish(&mut self) {
        if *self.sender.borrow() != self.value {
            time::sleep_until(self.last + PROGRESS_INTERVAL).await;
            self.sender.send_replace(self.value);
        }
    }
}
