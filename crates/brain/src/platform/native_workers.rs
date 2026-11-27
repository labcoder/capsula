use std::io;
use std::path::Path;
use std::sync::{Arc, atomic::AtomicBool};

use super::{Containment, ContainmentTier, Job, Mode};
use crate::{BrainError, config::Config, runtime::Brain};

use crate::engines::claude_code::lane::{Host, Lane};
use crate::{runtime::claude, speech, voice};

pub(crate) enum ClaudeSetting {
    Enabled,
    Disabled,
}

pub(crate) fn claude_setting(config: &Config, mode: Mode) -> ClaudeSetting {
    if !super::supports(mode) {
        return ClaudeSetting::Disabled;
    }
    let enabled = config.claude_code_enabled();
    tracing::info!(target: env!("CARGO_CRATE_NAME"), enabled, "Claude Code setting");
    if enabled {
        ClaudeSetting::Enabled
    } else {
        ClaudeSetting::Disabled
    }
}

pub(crate) struct Prepared {
    setting: ClaudeSetting,
    models: Option<std::path::PathBuf>,
    stuck: Vec<(kumochi_protocol::voice::VoicePart, voice::installer::Folder)>,
}

pub(crate) fn prepare(
    brain: &mut Brain,
    setting: ClaudeSetting,
    mode: Mode,
) -> Result<(Arc<Job>, Prepared), BrainError> {
    kernel_contained(mode)?;
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

/// First in `prepare`, so a mode without Job containment, a parent-bound Group included,
/// removes nothing and starts no worker.
fn kernel_contained(mode: Mode) -> Result<(), BrainError> {
    if super::containment(mode) == Containment::RuntimeEnabled(ContainmentTier::Kernel) {
        Ok(())
    } else {
        Err(BrainError::Helpers(io::ErrorKind::Unsupported.into()))
    }
}

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
    let lane = claude_host(vault_held, jobs, exe).and_then(|host| {
        Lane::start(host)
            .inspect_err(|error| {
                tracing::warn!(target: env!("CARGO_CRATE_NAME"), kind = ?error.kind(), "Claude Code's lane didn't start");
            })
            .ok()
    });
    let claude = match (setting, lane) {
        (ClaudeSetting::Enabled, Some(lane)) => claude::Claude::start(lane, claude::SIGN_IN),
        (ClaudeSetting::Enabled, None) => claude::Claude::default(),
        (ClaudeSetting::Disabled, lane) => claude::Claude::turned_off(lane),
    };
    #[cfg(any(test, windows))]
    brain.attach_claude(claude);
    #[cfg(not(any(test, windows)))]
    drop(claude);
}

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
fn claude_host(vault_held: Arc<AtomicBool>, jobs: &Arc<Job>, brain_exe: &Path) -> Option<Host> {
    use crate::engines::claude_code::install::{FindError, locate};
    use crate::engines::claude_code::lane::Program;

    let root = match kumochi_storage::local_app_data(kumochi_storage::Today::KnownFolder) {
        Ok(root) => root,
        Err(error) => {
            tracing::warn!(target: env!("CARGO_CRATE_NAME"), kind = ?error, "no local data folder for Claude Code");
            return None;
        }
    };
    let Ok(_root) = super::hold_chain(&root) else {
        tracing::warn!(target: env!("CARGO_CRATE_NAME"), "the local data root for Claude Code isn't trusted");
        return None;
    };
    let base = root.join(kumochi_protocol::constants::DATA_DIR);
    if let Err(error) = super::product_folder(&base) {
        tracing::warn!(target: env!("CARGO_CRATE_NAME"), kind = ?error.kind(), "couldn't make the local data folder for Claude Code");
        return None;
    }
    Some(Host {
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
    })
}

#[cfg(not(windows))]
fn claude_host(_: Arc<AtomicBool>, _: &Arc<Job>, _: &Path) -> Option<Host> {
    None
}

#[cfg(test)]
#[test]
fn only_job_containment_admits_native_workers() {
    for mode in [Mode::Detached, Mode::ParentBound] {
        let admitted = cfg!(windows) && mode == Mode::Detached;
        let result = kernel_contained(mode);
        assert_eq!(result.is_ok(), admitted, "{mode:?}");
        if !admitted {
            assert!(
                matches!(result, Err(BrainError::Helpers(error)) if error.kind() == io::ErrorKind::Unsupported)
            );
        }
    }
}

#[cfg(all(test, not(windows)))]
#[tokio::test]
async fn requested_native_workers_refuse_without_containment() {
    use std::io::ErrorKind;

    let scratch = crate::test_support::Scratch::new("native-worker-refusal");
    let mut brain = crate::test_support::brain(&scratch, "");
    let results = [Mode::Detached, Mode::ParentBound]
        .map(|mode| prepare(&mut brain, ClaudeSetting::Enabled, mode));
    brain.close().await;
    for result in results {
        assert!(
            matches!(result, Err(BrainError::Helpers(error)) if error.kind() == ErrorKind::Unsupported)
        );
    }
}

#[cfg(all(test, windows))]
#[tokio::test]
async fn the_running_mode_decides_the_claude_setting_and_the_helpers() {
    use kumochi_protocol::message::ThreadId;
    use std::io::ErrorKind;

    let scratch = crate::test_support::Scratch::new("native-worker-mode");
    let config = Config::load(Some(scratch.dir().to_owned()), ThreadId(1));
    assert!(matches!(
        claude_setting(&config, Mode::Detached),
        ClaudeSetting::Enabled
    ));
    assert!(matches!(
        claude_setting(&config, Mode::ParentBound),
        ClaudeSetting::Disabled
    ));
    let mut brain = crate::test_support::brain(&scratch, "");
    let result = prepare(&mut brain, ClaudeSetting::Enabled, Mode::ParentBound);
    brain.close().await;
    assert!(
        matches!(result, Err(BrainError::Helpers(error)) if error.kind() == ErrorKind::Unsupported),
        "a mode without containment refuses"
    );
}
