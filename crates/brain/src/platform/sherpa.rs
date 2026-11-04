//! Parakeet through sherpa-onnx's C API, loaded at run time under a fixed policy (see
//! `docs/development/voice.md`, "Speech input").
//!
//! Nothing here is `Send`: the libraries, the recognizer, its streams and their results all live
//! on the thread that loaded them, and each guard borrows from the one that made it, so the borrow
//! checker keeps the native destroy order: result, stream, recognizer, sherpa-onnx, ONNX Runtime.

mod abi;
#[cfg(test)]
mod tests;

use std::ffi::{CStr, CString, c_char};
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read as _};
use std::marker::PhantomData;
use std::mem;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use kumochi_protocol::voice::NATIVE_FILES;
use sha2::{Digest as _, Sha256};

use self::abi::{RawRecognizer, RawStream, RecognizerConfig, RecognizerResult};
use super::windows::{Library, RawExport, missing_dependency, modules, open_locked, system_dir};

/// The longest transcript accepted, in bytes.
// shortcut: a local bound until PR 1's MAX_TRANSCRIPT_BYTES lands; use that one then.
pub(crate) const MAX_TRANSCRIPT_BYTES: usize = 16 * 1024;

/// Decoder threads [S5].
const THREADS: i32 = 8;
const VERSION: &[u8] = b"2.0.1";
/// The v2.0.1 release's short commit, as its `SherpaOnnxGetGitSha1` reports it (read on Rez).
const GIT_SHA1: &[u8] = b"af2d8d81";
const SHERPA: &str = "sherpa-onnx-c-api.dll";
const ORT: &str = "onnxruntime.dll";
const MODEL_FILES: [&str; 4] = [
    "encoder.int8.onnx",
    "decoder.int8.onnx",
    "joiner.int8.onnx",
    "tokens.txt",
];

/// Every module the two pinned DLLs import by name, lower case, from the import tables in
/// `docs/development/voice-natives.md`; `measure_native_imports` keeps it honest.
pub(crate) const NATIVE_CLOSURE: [&str; 10] = [
    SHERPA,
    ORT,
    "msvcp140.dll",
    "msvcp140_1.dll",
    "vcruntime140.dll",
    "vcruntime140_1.dll",
    "ucrtbase.dll",
    "dbghelp.dll",
    "setupapi.dll",
    "dxgi.dll",
];

/// Why speech input's native side failed. It carries no native text and no paths.
#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeError {
    #[error("a native library is missing")]
    Missing,
    #[error("a native library doesn't match its pin")]
    Pin,
    #[error("a module the native libraries import is already loaded from another folder")]
    Conflict,
    #[error("a system library the native libraries need is missing")]
    Dependency,
    #[error("loading brought in a module from an unexpected folder")]
    Unexpected,
    #[error("a native library failed to load")]
    Load,
    #[error("a native library lacks an export")]
    Symbol,
    #[error("the native library isn't the pinned release")]
    Abi,
    #[error("the recognizer or a stream couldn't be created")]
    Create,
    #[error("the audio can't be handed to the recognizer")]
    Input,
    #[error("decoding gave no result")]
    Decode,
    #[error("the transcript is too long or isn't UTF-8")]
    Text,
}

type CreateRecognizer = unsafe extern "C" fn(*const RecognizerConfig) -> *const RawRecognizer;
type DestroyRecognizer = unsafe extern "C" fn(*const RawRecognizer);
type CreateStream = unsafe extern "C" fn(*const RawRecognizer) -> *const RawStream;
type DestroyStream = unsafe extern "C" fn(*const RawStream);
type AcceptWaveform = unsafe extern "C" fn(*const RawStream, i32, *const f32, i32);
type DecodeStream = unsafe extern "C" fn(*const RawRecognizer, *const RawStream);
type GetResult = unsafe extern "C" fn(*const RawStream) -> *const RecognizerResult;
type DestroyResult = unsafe extern "C" fn(*const RecognizerResult);
type StaticStr = unsafe extern "C" fn() -> *const c_char;

/// The closed table of the C functions Kumochi calls. Its pointers never leave `Sherpa`.
struct Fns {
    create_recognizer: CreateRecognizer,
    destroy_recognizer: DestroyRecognizer,
    create_stream: CreateStream,
    destroy_stream: DestroyStream,
    accept_waveform: AcceptWaveform,
    decode_stream: DecodeStream,
    get_result: GetResult,
    destroy_result: DestroyResult,
    version: StaticStr,
    git_sha1: StaticStr,
}

impl Fns {
    fn resolve(sherpa: &Library) -> Result<Self, NativeError> {
        let export = |name: &CStr| sherpa.export(name).ok_or(NativeError::Symbol);
        Ok(Self {
            create_recognizer: {
                let raw = export(c"SherpaOnnxCreateOfflineRecognizer")?;
                // SAFETY: c-api.h: `const SherpaOnnxOfflineRecognizer *
                // SherpaOnnxCreateOfflineRecognizer(const SherpaOnnxOfflineRecognizerConfig *)`.
                unsafe { mem::transmute::<RawExport, CreateRecognizer>(raw) }
            },
            destroy_recognizer: {
                let raw = export(c"SherpaOnnxDestroyOfflineRecognizer")?;
                // SAFETY: c-api.h: `void SherpaOnnxDestroyOfflineRecognizer(
                // const SherpaOnnxOfflineRecognizer *)`.
                unsafe { mem::transmute::<RawExport, DestroyRecognizer>(raw) }
            },
            create_stream: {
                let raw = export(c"SherpaOnnxCreateOfflineStream")?;
                // SAFETY: c-api.h: `const SherpaOnnxOfflineStream *SherpaOnnxCreateOfflineStream(
                // const SherpaOnnxOfflineRecognizer *)`.
                unsafe { mem::transmute::<RawExport, CreateStream>(raw) }
            },
            destroy_stream: {
                let raw = export(c"SherpaOnnxDestroyOfflineStream")?;
                // SAFETY: c-api.h: `void SherpaOnnxDestroyOfflineStream(
                // const SherpaOnnxOfflineStream *)`.
                unsafe { mem::transmute::<RawExport, DestroyStream>(raw) }
            },
            accept_waveform: {
                let raw = export(c"SherpaOnnxAcceptWaveformOffline")?;
                // SAFETY: c-api.h: `void SherpaOnnxAcceptWaveformOffline(
                // const SherpaOnnxOfflineStream *, int32_t, const float *, int32_t)`.
                unsafe { mem::transmute::<RawExport, AcceptWaveform>(raw) }
            },
            decode_stream: {
                let raw = export(c"SherpaOnnxDecodeOfflineStream")?;
                // SAFETY: c-api.h: `void SherpaOnnxDecodeOfflineStream(
                // const SherpaOnnxOfflineRecognizer *, const SherpaOnnxOfflineStream *)`.
                unsafe { mem::transmute::<RawExport, DecodeStream>(raw) }
            },
            get_result: {
                let raw = export(c"SherpaOnnxGetOfflineStreamResult")?;
                // SAFETY: c-api.h: `const SherpaOnnxOfflineRecognizerResult *
                // SherpaOnnxGetOfflineStreamResult(const SherpaOnnxOfflineStream *)`.
                unsafe { mem::transmute::<RawExport, GetResult>(raw) }
            },
            destroy_result: {
                let raw = export(c"SherpaOnnxDestroyOfflineRecognizerResult")?;
                // SAFETY: c-api.h: `void SherpaOnnxDestroyOfflineRecognizerResult(
                // const SherpaOnnxOfflineRecognizerResult *)`.
                unsafe { mem::transmute::<RawExport, DestroyResult>(raw) }
            },
            version: {
                let raw = export(c"SherpaOnnxGetVersionStr")?;
                // SAFETY: c-api.h: `const char *SherpaOnnxGetVersionStr()`.
                unsafe { mem::transmute::<RawExport, StaticStr>(raw) }
            },
            git_sha1: {
                let raw = export(c"SherpaOnnxGetGitSha1")?;
                // SAFETY: c-api.h: `const char *SherpaOnnxGetGitSha1()`.
                unsafe { mem::transmute::<RawExport, StaticStr>(raw) }
            },
        })
    }
}

/// sherpa-onnx and the ONNX Runtime it imports, loaded and checked.
pub(crate) struct Sherpa {
    fns: Fns,
    // Fields drop in order: sherpa-onnx before the ONNX Runtime it imports.
    _sherpa_dll: Option<Library>,
    _ort_dll: Option<Library>,
    _not_send: PhantomData<*const ()>,
}

impl Sherpa {
    /// Loads both pinned DLLs from `natives`. In order, stopping at the first failure: refuses if
    /// any module of their import closure is already loaded from an unexpected folder; checks
    /// each DLL's size and SHA-256 through a handle that stays open until it's loaded; loads ONNX
    /// Runtime, then sherpa-onnx, by full path with imports from the system folder only; checks
    /// that every new module is a pin or a system module; then resolves the exports and checks the
    /// release.
    pub(crate) fn load(natives: &Path) -> Result<Self, NativeError> {
        let natives = std::path::absolute(natives).map_err(|_| NativeError::Missing)?;
        let system = system_dir().map_err(|_| NativeError::Load)?;
        let before = modules().map_err(|_| NativeError::Load)?;
        check_closure(&before, &system, &natives)?;
        let ort = load_pinned(&natives, ORT)?;
        let sherpa = load_pinned(&natives, SHERPA)?;
        let after = modules().map_err(|_| NativeError::Unexpected)?;
        if check_closure(&after, &system, &natives).is_err()
            || !only_expected_added(&before, &after, &system, &natives)
        {
            return Err(NativeError::Unexpected);
        }
        let loaded = Self {
            fns: Fns::resolve(&sherpa)?,
            _sherpa_dll: Some(sherpa),
            _ort_dll: Some(ort),
            _not_send: PhantomData,
        };
        loaded.check_release()?;
        Ok(loaded)
    }

    fn check_release(&self) -> Result<(), NativeError> {
        // SAFETY: c-api.h's `SherpaOnnxGetVersionStr` takes nothing and returns a static string.
        let version = unsafe { (self.fns.version)() };
        // SAFETY: c-api.h's `SherpaOnnxGetGitSha1` takes nothing and returns a static string.
        let git_sha1 = unsafe { (self.fns.git_sha1)() };
        if version.is_null() || git_sha1.is_null() {
            return Err(NativeError::Abi);
        }
        // SAFETY: non-null, NUL-terminated and static, per c-api.h.
        let version = unsafe { CStr::from_ptr(version) };
        // SAFETY: non-null, NUL-terminated and static, per c-api.h.
        let git_sha1 = unsafe { CStr::from_ptr(git_sha1) };
        if version.to_bytes() != VERSION || git_sha1.to_bytes() != GIT_SHA1 {
            return Err(NativeError::Abi);
        }
        Ok(())
    }

    /// Creates Parakeet's recognizer from the model files in `models`.
    pub(crate) fn recognizer(&self, models: &Path) -> Result<Recognizer<'_>, NativeError> {
        let [encoder, decoder, joiner, tokens] = MODEL_FILES.map(|name| c_path(&models.join(name)));
        let (encoder, decoder, joiner, tokens) = (encoder?, decoder?, joiner?, tokens?);
        // SAFETY: the struct holds only integers, floats and raw pointers, for which all-zero is
        // valid, and the C API reads a zero field as its default.
        let mut config: RecognizerConfig = unsafe { mem::zeroed() };
        let model = &mut config.model_config;
        model.transducer.encoder = encoder.as_ptr();
        model.transducer.decoder = decoder.as_ptr();
        model.transducer.joiner = joiner.as_ptr();
        model.tokens = tokens.as_ptr();
        model.num_threads = THREADS;
        model.debug = 0;
        model.provider = c"cpu".as_ptr();
        model.model_type = c"nemo_transducer".as_ptr();
        config.decoding_method = c"greedy_search".as_ptr();
        // SAFETY: `config` matches c-api.h's layout and every string it points to outlives the call.
        let raw = unsafe { (self.fns.create_recognizer)(&raw const config) };
        let raw = NonNull::new(raw.cast_mut()).ok_or(NativeError::Create)?;
        Ok(Recognizer { sherpa: self, raw })
    }

    /// A table of test functions with no library behind it.
    #[cfg(test)]
    fn with_fns(fns: Fns) -> Self {
        Self {
            fns,
            _sherpa_dll: None,
            _ort_dll: None,
            _not_send: PhantomData,
        }
    }
}

/// Paths reach sherpa-onnx as UTF-8.
// shortcut: a non-UTF-8 models path is refused, and non-ASCII paths are untested; test them
// before speech input ships to a user whose profile folder isn't ASCII.
fn c_path(path: &Path) -> Result<CString, NativeError> {
    let path = path.to_str().ok_or(NativeError::Input)?;
    CString::new(path).map_err(|_| NativeError::Input)
}

/// Parakeet's recognizer. Dropping it destroys the native recognizer.
pub(crate) struct Recognizer<'s> {
    sherpa: &'s Sherpa,
    raw: NonNull<RawRecognizer>,
}

impl Recognizer<'_> {
    /// Transcribes one utterance of mono PCM at its capture rate; sherpa-onnx resamples it.
    pub(crate) fn decode(&self, rate: u32, samples: &[i16]) -> Result<String, NativeError> {
        let rate = i32::try_from(rate)
            .ok()
            .filter(|&rate| rate > 0)
            .ok_or(NativeError::Input)?;
        let count = sample_count(samples.len())?;
        let wave: Vec<f32> = samples
            .iter()
            .map(|&sample| f32::from(sample) / 32_768.0)
            .collect();
        let stream = self.stream()?;
        let fns = &self.sherpa.fns;
        // SAFETY: c-api.h allows one accept per offline stream, and this stream is new; `wave`
        // holds `count` samples and outlives the call.
        unsafe { (fns.accept_waveform)(stream.raw.as_ptr(), rate, wave.as_ptr(), count) };
        // SAFETY: the recognizer and its stream are both live.
        unsafe { (fns.decode_stream)(self.raw.as_ptr(), stream.raw.as_ptr()) };
        stream.result()?.text()
    }

    fn stream(&self) -> Result<Stream<'_>, NativeError> {
        // SAFETY: the recognizer is live.
        let raw = unsafe { (self.sherpa.fns.create_stream)(self.raw.as_ptr()) };
        let raw = NonNull::new(raw.cast_mut()).ok_or(NativeError::Create)?;
        Ok(Stream {
            recognizer: self,
            raw,
        })
    }
}

impl Drop for Recognizer<'_> {
    fn drop(&mut self) {
        // SAFETY: created by this table's create_recognizer, destroyed once; every stream
        // borrowed it, so none is left.
        unsafe { (self.sherpa.fns.destroy_recognizer)(self.raw.as_ptr()) };
    }
}

/// A sample count as the C API takes it: positive and within `i32`.
fn sample_count(len: usize) -> Result<i32, NativeError> {
    i32::try_from(len)
        .ok()
        .filter(|&count| count > 0)
        .ok_or(NativeError::Input)
}

struct Stream<'r> {
    recognizer: &'r Recognizer<'r>,
    raw: NonNull<RawStream>,
}

impl Stream<'_> {
    fn result(&self) -> Result<ResultGuard<'_>, NativeError> {
        // SAFETY: the stream is live and has been decoded.
        let raw = unsafe { (self.recognizer.sherpa.fns.get_result)(self.raw.as_ptr()) };
        let raw = NonNull::new(raw.cast_mut()).ok_or(NativeError::Decode)?;
        Ok(ResultGuard { stream: self, raw })
    }
}

impl Drop for Stream<'_> {
    fn drop(&mut self) {
        // SAFETY: created by this table's create_stream and destroyed once; its result borrowed
        // it, so that's gone.
        unsafe { (self.recognizer.sherpa.fns.destroy_stream)(self.raw.as_ptr()) };
    }
}

struct ResultGuard<'t> {
    stream: &'t Stream<'t>,
    raw: NonNull<RecognizerResult>,
}

impl ResultGuard<'_> {
    /// The transcript, copied out and checked.
    fn text(&self) -> Result<String, NativeError> {
        // SAFETY: the result is live and laid out as c-api.h declares.
        let text = unsafe { self.raw.as_ref() }.text;
        if text.is_null() {
            return Err(NativeError::Decode);
        }
        // SAFETY: `text` is NUL-terminated and owned by the result, which outlives this borrow.
        let text = unsafe { CStr::from_ptr(text) }.to_bytes();
        if text.len() > MAX_TRANSCRIPT_BYTES {
            return Err(NativeError::Text);
        }
        String::from_utf8(text.to_vec()).map_err(|_| NativeError::Text)
    }
}

impl Drop for ResultGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: returned by this table's get_result and destroyed once.
        unsafe { (self.stream.recognizer.sherpa.fns.destroy_result)(self.raw.as_ptr()) };
    }
}

/// Verifies one pinned DLL and loads it while the verified handle still holds its bytes.
fn load_pinned(natives: &Path, name: &str) -> Result<Library, NativeError> {
    let pin = NATIVE_FILES
        .iter()
        .find(|file| file.name == name)
        .expect("both natives are pinned in the manifest");
    let path = natives.join(name);
    let held = verify(&path, pin.bytes, pin.sha256)?;
    let loaded = Library::load(&path).map_err(|error| {
        if missing_dependency(&error) {
            NativeError::Dependency
        } else {
            NativeError::Load
        }
    });
    drop(held);
    loaded
}

/// Opens `path` against writers and checks its size and SHA-256 through that handle.
fn verify(path: &Path, bytes: u64, sha256: &str) -> Result<File, NativeError> {
    let mut file = open_locked(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => NativeError::Missing,
        _ => NativeError::Load,
    })?;
    if file.metadata().map_err(|_| NativeError::Load)?.len() != bytes {
        return Err(NativeError::Pin);
    }
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1 << 16];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => hash.update(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(NativeError::Load),
        }
    }
    let mut hex = String::with_capacity(64);
    for byte in hash.finalize() {
        write!(hex, "{byte:02x}").expect("writing to a String can't fail");
    }
    if hex != sha256 {
        return Err(NativeError::Pin);
    }
    Ok(file)
}

/// Refuses when a module named like one in [`NATIVE_CLOSURE`] is loaded from anywhere but its
/// expected place: the pins exactly at `natives`, everything else directly in `system`. Windows
/// binds an import to a loaded module of that name whatever folder it came from.
pub(crate) fn check_closure(
    loaded: &[PathBuf],
    system: &Path,
    natives: &Path,
) -> Result<(), NativeError> {
    for path in loaded {
        let Some(name) = path.file_name() else {
            continue;
        };
        let name = name.to_string_lossy().to_lowercase();
        if !NATIVE_CLOSURE.contains(&name.as_str()) {
            continue;
        }
        let expected = if name == SHERPA || name == ORT {
            same_path(path, &natives.join(&name))
        } else {
            path.parent().is_some_and(|dir| same_path(dir, system))
        };
        if !expected {
            return Err(NativeError::Conflict);
        }
    }
    Ok(())
}

/// Whether every module in `after` but not `before` is a pin at `natives` or a system module.
pub(crate) fn only_expected_added(
    before: &[PathBuf],
    after: &[PathBuf],
    system: &Path,
    natives: &Path,
) -> bool {
    let pins = [natives.join(SHERPA), natives.join(ORT)];
    after
        .iter()
        .filter(|path| !before.iter().any(|old| same_path(old, path)))
        .all(|path| {
            pins.iter().any(|pin| same_path(pin, path))
                || path.parent().is_some_and(|dir| same_path(dir, system))
        })
}

/// Windows paths compare without case.
fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}
