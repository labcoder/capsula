//! Parakeet through sherpa-onnx's C API, loaded at run time under a fixed policy (see
//! `docs/development/voice.md`, "Speech input").
//!
//! Nothing here is `Send`: the libraries, the recognizer, its streams and their results all live
//! on the thread that loaded them, and each guard borrows from the one that made it, so the borrow
//! checker keeps the native destroy order: result, stream, recognizer, sherpa-onnx, ONNX Runtime.

mod abi;
#[cfg(test)]
pub(crate) mod tests;

use std::ffi::{CStr, CString, c_char};
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::marker::PhantomData;
use std::mem;
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use kumochi_protocol::constants;
use kumochi_protocol::voice::{NATIVE_FILES, NativeFile};
use sha2::{Digest as _, Sha256};

use self::abi::{RawRecognizer, RawStream, RecognizerConfig, RecognizerResult};
use super::windows::{
    Library, RawExport, Untrusted, hold_chain, hold_file, local_app_data, missing_dependency,
    modules, open_locked, owner_only_dir, system_dir,
};

/// The longest transcript accepted, in bytes.
// shortcut: a local bound until PR 1's MAX_TRANSCRIPT_BYTES lands; use that one then.
pub(crate) const MAX_TRANSCRIPT_BYTES: usize = 16 * 1024;

/// Decoder threads [S5].
const THREADS: i32 = 8;
const VERSION: &[u8] = b"2.0.1";
/// The v2.0.1 release's short commit, as its `SherpaOnnxGetGitSha1` reports it (read on Rez).
const GIT_SHA1: &[u8] = b"af2d8d81";
const SHERPA: &str = "sherpa-onnx-c-api.dll";
/// Where the pins are staged, under the trusted root.
const STAGE: [&str; 2] = ["natives", "sherpa-onnx-2.0.1"];
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
#[derive(
    Debug, thiserror::Error, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
pub enum NativeError {
    /// A native library is missing.
    #[error("a native library is missing")]
    Missing,
    /// A folder or native library on the load path is a reparse point or resolves elsewhere.
    #[error("a folder or native library on the load path is a reparse point or resolves elsewhere")]
    Reparse,
    /// Someone other than this user or the system can change the load path.
    #[error("someone other than this user or the system can change the load path")]
    Unsafe,
    /// A native library doesn't match its pin.
    #[error("a native library doesn't match its pin")]
    Pin,
    /// A module the native libraries import is already loaded from another folder.
    #[error("a module the native libraries import is already loaded from another folder")]
    Conflict,
    /// A system library the native libraries need is missing.
    #[error("a system library the native libraries need is missing")]
    Dependency,
    /// Loading brought in a module from an unexpected folder.
    #[error("loading brought in a module from an unexpected folder")]
    Unexpected,
    /// A native library failed to load.
    #[error("a native library failed to load")]
    Load,
    /// A native library lacks an export.
    #[error("a native library lacks an export")]
    Symbol,
    /// The native library isn't the pinned release.
    #[error("the native library isn't the pinned release")]
    Abi,
    /// The recognizer or a stream couldn't be created.
    #[error("the recognizer or a stream couldn't be created")]
    Create,
    /// The audio can't be handed to the recognizer.
    #[error("the audio can't be handed to the recognizer")]
    Input,
    /// Decoding gave no result.
    #[error("decoding gave no result")]
    Decode,
    /// The transcript is too long or isn't UTF-8.
    #[error("the transcript is too long or isn't UTF-8")]
    Text,
}

type CreateRecognizer =
    unsafe extern "C-unwind" fn(*const RecognizerConfig) -> *const RawRecognizer;
type DestroyRecognizer = unsafe extern "C-unwind" fn(*const RawRecognizer);
type CreateStream = unsafe extern "C-unwind" fn(*const RawRecognizer) -> *const RawStream;
type DestroyStream = unsafe extern "C-unwind" fn(*const RawStream);
type AcceptWaveform = unsafe extern "C-unwind" fn(*const RawStream, i32, *const f32, i32);
type DecodeStream = unsafe extern "C-unwind" fn(*const RawRecognizer, *const RawStream);
type GetResult = unsafe extern "C-unwind" fn(*const RawStream) -> *const RecognizerResult;
type DestroyResult = unsafe extern "C-unwind" fn(*const RecognizerResult);
type StaticStr = unsafe extern "C-unwind" fn() -> *const c_char;

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
    /// Loads both pinned DLLs, staged from `source` into `root` (see [`stage`]). In order,
    /// stopping at the first failure: refuses if any module of their import closure is already
    /// loaded from an unexpected folder; stages and holds them; loads ONNX Runtime, then sherpa-onnx,
    /// by full path with imports from the system folder only, while every folder above them and
    /// their verified handles stay held; checks that every new module is a pin or a system module;
    /// then resolves the exports and checks the release.
    pub(crate) fn load(source: &Path, root: &Path) -> Result<Self, NativeError> {
        let system = system_dir().map_err(|_| NativeError::Load)?;
        let before = modules().map_err(|_| NativeError::Load)?;
        let natives = STAGE
            .iter()
            .fold(root.to_owned(), |dir, name| dir.join(name));
        check_closure(&before, &system, &natives)?;
        let staged = stage(source, root, NATIVE_FILES)?;
        let ort = staged.load(ORT)?;
        let sherpa = staged.load(SHERPA)?;
        drop(staged);
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
pub(crate) fn c_path(path: &Path) -> Result<CString, NativeError> {
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

/// The trusted root natives are staged under: Kumochi's folder in the local application data
/// folder the shell reports, never the environment's `LOCALAPPDATA`.
pub(crate) fn staging_root() -> Result<PathBuf, NativeError> {
    let root = local_app_data()
        .map_err(|_| NativeError::Unsafe)?
        .join(constants::DATA_DIR);
    match std::fs::create_dir(&root) {
        Ok(()) => Ok(root),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(root),
        Err(_) => Err(NativeError::Load),
    }
}

/// The pins staged in an owner-only folder. Every folder from the volume root down to it, and
/// each staged DLL, stays open until this drops, so none of their names can be rebound.
pub(crate) struct Staged {
    dir: PathBuf,
    // Fields drop in order: the files before the folders above them.
    files: Vec<(&'static str, File)>,
    _folders: Vec<File>,
}

impl Staged {
    /// Loads the staged DLL `name` by full path while its verified handle is held.
    fn load(&self, name: &str) -> Result<Library, NativeError> {
        if !self.files.iter().any(|(staged, _)| *staged == name) {
            return Err(NativeError::Missing);
        }
        Library::load(&self.dir.join(name)).map_err(|error| {
            if missing_dependency(&error) {
                NativeError::Dependency
            } else {
                NativeError::Load
            }
        })
    }
}

/// Stages `pins` from `source` into `root\natives\sherpa-onnx-2.0.1`. `root` and every folder
/// above it must be trusted (see `platform::windows::protected`); the two staging folders are
/// made owner-only. A staged file that matches its pin is kept; any other is replaced by a copy
/// read through a source handle that refuses writers and checked while it streams. Each staged
/// file is then opened against writers and deleters, checked and verified through that handle.
pub(crate) fn stage(
    source: &Path,
    root: &Path,
    pins: &[NativeFile],
) -> Result<Staged, NativeError> {
    let mut folders = hold_chain(root).map_err(untrusted)?;
    let mut dir = root.to_owned();
    for name in STAGE {
        dir.push(name);
        folders.push(owner_only_dir(&dir).map_err(untrusted)?);
    }
    let mut files = Vec::with_capacity(pins.len());
    for pin in pins {
        let path = dir.join(pin.name);
        let held = match hold_verified(&path, pin) {
            Err(NativeError::Missing | NativeError::Pin) => {
                copy_pin(&source.join(pin.name), &path, pin)?;
                hold_verified(&path, pin)?
            }
            held => held?,
        };
        files.push((pin.name, held));
    }
    Ok(Staged {
        dir,
        files,
        _folders: folders,
    })
}

fn untrusted(error: Untrusted) -> NativeError {
    match error {
        Untrusted::Missing => NativeError::Missing,
        Untrusted::Reparse => NativeError::Reparse,
        Untrusted::Unsafe => NativeError::Unsafe,
        Untrusted::Io => NativeError::Load,
    }
}

fn hold_verified(path: &Path, pin: &NativeFile) -> Result<File, NativeError> {
    let mut file = hold_file(path).map_err(untrusted)?;
    check_pin(&mut file, pin.bytes, pin.sha256, None)?;
    Ok(file)
}

/// Copies `source` to the new file `path`, checking it against `pin` as it streams; a copy
/// that fails or doesn't match is removed.
fn copy_pin(source: &Path, path: &Path, pin: &NativeFile) -> Result<(), NativeError> {
    let mut from = open_locked(source).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => NativeError::Missing,
        _ => NativeError::Load,
    })?;
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(NativeError::Load),
    }
    let mut to = File::options()
        .write(true)
        .create_new(true)
        .share_mode(0)
        .open(path)
        .map_err(|_| NativeError::Load)?;
    let copied = check_pin(&mut from, pin.bytes, pin.sha256, Some(&mut to))
        .and_then(|()| to.sync_all().map_err(|_| NativeError::Load));
    drop(to);
    if copied.is_err() {
        std::fs::remove_file(path).ok();
    }
    copied
}

/// Opens `path` against writers and checks its size and SHA-256 through that handle.
#[cfg(test)]
fn verify(path: &Path, bytes: u64, sha256: &str) -> Result<File, NativeError> {
    let mut file = open_locked(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => NativeError::Missing,
        _ => NativeError::Load,
    })?;
    check_pin(&mut file, bytes, sha256, None)?;
    Ok(file)
}

/// Reads `file` to its end, also into `copy` when given, and checks its size and SHA-256.
fn check_pin(
    file: &mut File,
    bytes: u64,
    sha256: &str,
    mut copy: Option<&mut File>,
) -> Result<(), NativeError> {
    if file.metadata().map_err(|_| NativeError::Load)?.len() != bytes {
        return Err(NativeError::Pin);
    }
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1 << 16];
    loop {
        let count = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(NativeError::Load),
        };
        hash.update(&buffer[..count]);
        if let Some(copy) = copy.as_deref_mut() {
            copy.write_all(&buffer[..count])
                .map_err(|_| NativeError::Load)?;
        }
    }
    let mut hex = String::with_capacity(64);
    for byte in hash.finalize() {
        write!(hex, "{byte:02x}").expect("writing to a String can't fail");
    }
    if hex != sha256 {
        return Err(NativeError::Pin);
    }
    Ok(())
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
