use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use kumochi_protocol::{BUILD_ID, Secret};

use super::{HANDSHAKE_TIMEOUT, Hello, Identity, Incarnation, Pipes, Session, Tap, timeout};
use crate::runtime::audio_fixture;
use crate::scratch;

/// Starts an offline Brain and its real authenticated named-pipe pair for a same-image test.
/// The join reports whether the paired session ended at the Brain.
///
/// # Panics
/// Panics if the private test namespace or the fixture Brain cannot start.
pub fn offline_audio_peer(script: String) -> (u32, Hello, JoinHandle<bool>) {
    offline_peer(script, 8, None)
}

/// Starts an authenticated offline peer with explicit test barriers on control and data credit.
/// # Panics
/// Panics if the private test namespace or the fixture Brain cannot start.
#[must_use]
pub fn offline_plain_peer(
    script: String,
    tone_chunks: usize,
) -> (
    u32,
    Hello,
    std::sync::Arc<audio_fixture::AudioProbe>,
    JoinHandle<bool>,
) {
    let probe = std::sync::Arc::new(audio_fixture::AudioProbe::default());
    let (session, hello, task) =
        offline_peer(script, tone_chunks, Some(std::sync::Arc::clone(&probe)));
    (session, hello, probe, task)
}

/// Starts the real paired Brain with the offline replay's narrated step.
/// # Panics
/// Panics if the private test namespace or the fixture Brain cannot start.
pub fn offline_narrated_peer(
    tone_chunks: usize,
) -> (
    u32,
    Hello,
    std::sync::Arc<audio_fixture::AudioProbe>,
    JoinHandle<bool>,
) {
    offline_draw_peer(tone_chunks, audio_fixture::AudioProbe::narrated())
}

/// Starts one same-run narrated replacement with controlled initial data credit.
/// # Panics
/// Panics if the private test namespace or the fixture Brain cannot start.
pub fn offline_narrated_replacement_peer(
    credit: u32,
) -> (
    u32,
    Hello,
    std::sync::Arc<audio_fixture::AudioProbe>,
    JoinHandle<bool>,
) {
    offline_draw_peer(8, audio_fixture::AudioProbe::replacing(credit))
}

/// Starts a narrated replacement chain after returning the initial painted prefix's credit.
/// # Panics
/// Panics if the private test namespace or the fixture Brain cannot start.
pub fn offline_narrated_replacement_chain_peer() -> (
    u32,
    Hello,
    std::sync::Arc<audio_fixture::AudioProbe>,
    JoinHandle<bool>,
) {
    offline_draw_peer(8, audio_fixture::AudioProbe::chaining_replacements())
}

/// Starts a narrated turn followed by a silent replacement on the same pair.
/// # Panics
/// Panics if the private test namespace or the fixture Brain cannot start.
pub fn offline_narrated_stopped_replacement_peer() -> (
    u32,
    Hello,
    std::sync::Arc<audio_fixture::AudioProbe>,
    JoinHandle<bool>,
) {
    offline_draw_peer(8, audio_fixture::AudioProbe::stopped_replacement())
}

/// Starts 257 authored-only turns followed by a narrated turn on the same pair.
/// # Panics
/// Panics if the private test namespace or the fixture Brain cannot start.
pub fn offline_authored_then_narrated_peer() -> (
    u32,
    Hello,
    std::sync::Arc<audio_fixture::AudioProbe>,
    JoinHandle<bool>,
) {
    offline_draw_peer(8, audio_fixture::AudioProbe::authored_then_narrated())
}

fn offline_draw_peer(
    tone_chunks: usize,
    probe: audio_fixture::AudioProbe,
) -> (
    u32,
    Hello,
    std::sync::Arc<audio_fixture::AudioProbe>,
    JoinHandle<bool>,
) {
    let probe = std::sync::Arc::new(probe);
    let (session, hello, task) = offline_peer(
        String::new(),
        tone_chunks,
        Some(std::sync::Arc::clone(&probe)),
    );
    (session, hello, probe, task)
}

fn offline_peer(
    script: String,
    tone_chunks: usize,
    probe: Option<std::sync::Arc<audio_fixture::AudioProbe>>,
) -> (u32, Hello, JoinHandle<bool>) {
    let session = getrandom::u32().expect("private test pipe name") | (1 << 31);
    let mut capability = [0; kumochi_protocol::message::CAPABILITY_LEN];
    getrandom::fill(&mut capability).expect("in-memory test capability");
    let identity = Identity {
        capability: Secret::new(capability),
        incarnation: Incarnation(1),
        shell_image: std::env::current_exe().expect("the actual test shell image"),
    };
    let hello = Hello {
        build_id: BUILD_ID.to_owned(),
        capability: identity.capability.clone(),
    };
    let (ready, waiting) = mpsc::sync_channel(1);
    let deadline = if probe.as_ref().is_some_and(|probe| probe.many_turns()) {
        Duration::from_secs(120)
    } else {
        HANDSHAKE_TIMEOUT * 3
    };
    let task = thread::Builder::new()
        .name("audio-fixture-brain".into())
        .spawn(move || {
            if let Some(probe) = probe {
                audio_fixture::install(probe);
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("one runtime in the Brain fixture");
            runtime.block_on(async {
                let scratch = scratch::dir("paired-queued-audio");
                let mut brain = audio_fixture::brain(scratch.path(), &script, tone_chunks).await;
                let mut pipes = Pipes::create(session).expect("owner-only test pipes");
                ready.send(()).expect("waiting test peer");
                let mut tap = Tap::default();
                let result = timeout(deadline, async {
                    pipes
                        .control
                        .connect()
                        .await
                        .expect("fixture control client");
                    pipes.session(&identity, &mut brain, &mut tap).await
                })
                .await;
                drop(pipes);
                brain.close().await;
                matches!(result, Ok(Ok(Session::ShellLost)))
            })
        })
        .expect("start the offline Brain thread");
    waiting
        .recv_timeout(HANDSHAKE_TIMEOUT)
        .expect("the Brain owns both test pipes");
    (session, hello, task)
}
