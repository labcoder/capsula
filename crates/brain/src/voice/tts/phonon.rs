//! The real engine: Phonon on xn's CPU pool. CI never loads it; tests use a fake engine.
//!
//! shortcut: xn, xn-gemm and tokenizers read the nonsecret numeric tuning variables
//! `XN_SPIN_BUDGET`, `XN_Q8_REPACK`, `GEMM_PACKED_LHS_CACHE`, `GEMM_PACKED_LHS_CACHE_MB` and
//! `TOKENIZERS_PARALLELISM` from the process environment (an exception the Captain granted).
//! Kumochi never sets or unsets them. Upgrade when upstream takes explicit configuration for each.

use std::cell::Cell;
use std::io;
use std::path::Path;
use std::sync::OnceLock;

use ptts::checkpoint::read_config;
use ptts::preprocess::Lang;
use ptts::synth::{DeviceKind, Quant, SpeechStream, Synth};
use xn::threadpool::PoolConfig;

use crate::platform::ModelFiles;

use super::{CONFIG, Engine, SAMPLE_RATE, TOKENIZER, TtsError, VOICE, WEIGHTS};

#[cfg(test)]
mod tests;

const POOL_WIDTH: usize = 8;
const WARM_UP: &str = "Hi.";

pub(super) type Job = Box<dyn FnOnce() + Send + 'static>;

/// xn's operator pool is process-wide and built once, so its outcome is kept for the process:
/// a pool that came up short can't be rebuilt by a later handle.
static POOL: OnceLock<Result<(), TtsError>> = OnceLock::new();

fn pool() -> Result<(), TtsError> {
    *POOL.get_or_init(|| {
        start_pool(
            |config, spawn| xn::threadpool::start_pool_with(config, spawn),
            xn::threadpool::size,
            // xn's workers never return, so their handles are detached.
            |job| {
                std::thread::Builder::new()
                    .name("xn-worker".into())
                    .spawn(job)
                    .map(drop)
            },
        )
    })
}

/// Sizes xn's pool without its environment-writing setter, and refuses to run on a pool that
/// isn't exactly 8 wide or lost a worker, since a missing worker would hang the first operator.
pub(super) fn start_pool(
    start: impl FnOnce(PoolConfig, &dyn Fn(Job)) -> usize,
    size: impl FnOnce() -> usize,
    spawn: impl Fn(Job) -> io::Result<()>,
) -> Result<(), TtsError> {
    let failed = Cell::new(false);
    let width = start(
        PoolConfig {
            workers: POOL_WIDTH - 1,
            spin_budget: Some(5_000),
        },
        &|job| {
            if spawn(job).is_err() {
                failed.set(true);
            }
        },
    );
    if failed.get() || width != POOL_WIDTH || size() != POOL_WIDTH {
        tracing::warn!(width, "spoken replies' compute pool is unavailable");
        return Err(TtsError::Load);
    }
    Ok(())
}

pub(super) struct Phonon {
    synth: Synth,
    _models: ModelFiles,
}

impl Phonon {
    pub(super) fn load(dir: &Path) -> Result<Self, TtsError> {
        let models = ModelFiles::open(dir, &[CONFIG, WEIGHTS, TOKENIZER, VOICE])
            .map_err(|_| TtsError::ModelPath)?;
        pool()?;
        let config = read_config(dir.join(CONFIG)).map_err(|_| TtsError::Load)?;
        let synth = Synth::builder(config, dir.join(WEIGHTS), Lang::En)
            .tokenizer_file(dir.join(TOKENIZER))
            .device(DeviceKind::Cpu)
            .quant(Quant::Q80)
            .add_voice("juniper", dir.join(VOICE))
            .voice("juniper")
            .seed(42)
            .build()
            .map_err(|_| TtsError::Load)?;
        if synth.sample_rate() != SAMPLE_RATE {
            return Err(TtsError::Load);
        }
        let phonon = Self {
            synth,
            _models: models,
        };
        for frame in phonon.stream(WARM_UP).map_err(|_| TtsError::Load)? {
            frame.map_err(|_| TtsError::Load)?;
        }
        Ok(phonon)
    }
}

impl Engine for Phonon {
    type Stream =
        std::iter::Map<SpeechStream, fn(ptts::Result<Vec<f32>>) -> Result<Vec<f32>, TtsError>>;

    fn stream(&self, text: &str) -> Result<Self::Stream, TtsError> {
        let stream = self.synth.stream(text).map_err(|_| TtsError::Synthesis)?;
        Ok(stream.map(|frame| frame.map_err(|_| TtsError::Synthesis)))
    }
}
