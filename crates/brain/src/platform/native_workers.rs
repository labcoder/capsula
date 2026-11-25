use std::path::Path;
use std::sync::{Arc, atomic::AtomicBool};

use super::Job;
use crate::{BrainError, config::Config, runtime::Brain};

#[cfg(windows)]
use crate::{engines, runtime::claude, speech, voice};

pub(crate) enum ClaudeSetting {
    #[cfg(any(test, windows))]
    Enabled,
    Disabled,
}

#[cfg(windows)]
pub(crate) fn claude_setting(config: &Config) -> ClaudeSetting {
    let enabled = config.claude_code_enabled();
    tracing::info!(target: env!("CARGO_CRATE_NAME"), enabled, "Claude Code setting");
    if enabled {
        ClaudeSetting::Enabled
    } else {
        ClaudeSetting::Disabled
    }
}

#[cfg(not(windows))]
pub(crate) fn claude_setting(_: &Config) -> ClaudeSetting {
    ClaudeSetting::Disabled
}

pub(crate) struct Prepared {
    #[cfg(windows)]
    setting: ClaudeSetting,
    #[cfg(windows)]
    models: Option<std::path::PathBuf>,
    #[cfg(windows)]
    stuck: Vec<(kumochi_protocol::voice::VoicePart, voice::installer::Folder)>,
}

#[cfg(windows)]
pub(crate) fn prepare(
    brain: &mut Brain,
    setting: ClaudeSetting,
) -> Result<(Arc<Job>, Prepared), BrainError> {
    let models = super::data_dir().map(|data| data.join(kumochi_protocol::voice::MODELS_DIR));
    let stuck = models
        .as_deref()
        .map(|models| brain.finish_removals(models))
        .unwrap_or_default();
    let jobs = Arc::new(Job::new().map_err(BrainError::Helpers)?);
    Ok((
        jobs,
        Prepared {
            setting,
            models,
            stuck,
        },
    ))
}

#[cfg(not(windows))]
pub(crate) fn prepare(_: &mut Brain, _: ClaudeSetting) -> Result<(Arc<Job>, Prepared), BrainError> {
    let jobs = Arc::new(Job::new().map_err(BrainError::Helpers)?);
    Ok((jobs, Prepared {}))
}

#[cfg(windows)]
pub(crate) fn start(
    prepared: Prepared,
    brain: &mut Brain,
    vault_held: Arc<AtomicBool>,
    exe: &Path,
    jobs: &Arc<Job>,
) {
    let Prepared {
        setting,
        models,
        stuck,
    } = prepared;
    if let Some(models) = models {
        start_voice(brain, models, &stuck, exe, jobs);
    }
    let lane = claude_lane(vault_held, jobs, exe);
    brain.attach_claude(match (setting, lane) {
        (ClaudeSetting::Enabled, Some(lane)) => claude::Claude::start(lane, claude::SIGN_IN),
        (ClaudeSetting::Enabled, None) => claude::Claude::default(),
        (ClaudeSetting::Disabled, lane) => claude::Claude::turned_off(lane),
    });
}

#[cfg(not(windows))]
pub(crate) fn start(
    _: Prepared,
    _: &mut Brain,
    _vault_held: Arc<AtomicBool>,
    _: &Path,
    _: &Arc<Job>,
) {
}

#[cfg(windows)]
fn start_voice(
    brain: &mut Brain,
    models: std::path::PathBuf,
    stuck: &[(kumochi_protocol::voice::VoicePart, voice::installer::Folder)],
    exe: &Path,
    jobs: &Arc<Job>,
) {
    use voice::Reports;
    use voice::stt::SttHandle;
    use voice::tts::TtsHandle;

    let runtime = tokio::runtime::Handle::current();
    let natives = exe
        .parent()
        .expect("the brain's path names a file in a folder")
        .to_path_buf();
    let (stt_reports, stt_heard) = Reports::new();
    let stt = speech::SpeechSupervisor::start(exe.to_path_buf(), Arc::clone(jobs))
        .and_then(|speech| SttHandle::spawn(runtime.clone(), speech, natives, stt_reports))
        .inspect_err(|error| {
            tracing::warn!(target: env!("CARGO_CRATE_NAME"), kind = ?error.kind(), "speech input can't start");
        })
        .ok();
    let (tts_reports, tts_heard) = Reports::new();
    let tts = TtsHandle::spawn(runtime, tts_reports)
        .inspect_err(|error| {
            tracing::warn!(target: env!("CARGO_CRATE_NAME"), kind = ?error.kind(), "spoken replies can't start");
        })
        .ok();
    let fetcher = Arc::new(voice::download::Online(models.clone()));
    brain.attach_voice(models, stuck, (stt, tts), [stt_heard, tts_heard], fetcher);
}

#[cfg(windows)]
fn claude_lane(
    vault_held: Arc<AtomicBool>,
    jobs: &Arc<Job>,
    brain_exe: &Path,
) -> Option<engines::claude_code::lane::Lane> {
    use engines::claude_code::install::{FindError, locate};
    use engines::claude_code::lane::{Host, Lane, Program};

    let base = match super::local_app_data() {
        Ok(local) => local.join(kumochi_protocol::constants::DATA_DIR),
        Err(error) => {
            tracing::warn!(target: env!("CARGO_CRATE_NAME"), kind = ?error.kind(), "no local data folder for Claude Code");
            return None;
        }
    };
    if let Err(error) = std::fs::create_dir(&base)
        && error.kind() != std::io::ErrorKind::AlreadyExists
    {
        tracing::warn!(target: env!("CARGO_CRATE_NAME"), kind = ?error.kind(), "couldn't make the local data folder for Claude Code");
        return None;
    }
    let host = Host {
        find: Box::new(|| {
            let profile = super::profile_dir().map_err(|_| FindError::NotFound)?;
            locate(&profile).map(|exe| Program {
                exe,
                prefix: Vec::new(),
            })
        }),
        base,
        vault_held,
        managed: Box::new(super::managed_presence),
        brain_job: Arc::clone(jobs),
        console: super::Stdio::Console,
        terminate: Job::terminate,
        brain_exe: brain_exe.to_owned(),
        runtime: tokio::runtime::Handle::current(),
    };
    Lane::start(host)
        .inspect_err(|error| {
            tracing::warn!(target: env!("CARGO_CRATE_NAME"), kind = ?error.kind(), "Claude Code's lane didn't start");
        })
        .ok()
}

#[cfg(all(test, not(windows)))]
#[tokio::test]
async fn requested_native_workers_refuse_without_containment() {
    use std::io::ErrorKind;

    let scratch = crate::test_support::Scratch::new("native-worker-refusal");
    let mut brain = crate::test_support::brain(&scratch, "");
    let result = prepare(&mut brain, ClaudeSetting::Enabled);
    brain.close().await;
    assert!(
        matches!(result, Err(BrainError::Helpers(error)) if error.kind() == ErrorKind::Unsupported)
    );
}
