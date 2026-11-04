//! The FFI owner's guards and error paths against a fake C table, the loader's pins and
//! closure policy, and Rez-only checks against the real DLLs.

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char};
use std::fmt::Write as _;
use std::fs::File;
use std::io;
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::path::{Path, PathBuf};

use kumochi_protocol::voice::NativeFile;
use sha2::{Digest as _, Sha256};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, WRITE_DAC,
    WRITE_OWNER,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_MOUNT_POINT;

use super::super::windows::{dacl_sddl, final_path_matches, local_app_data};
use super::abi::{LAYOUT, RawRecognizer, RawStream, RecognizerConfig, RecognizerResult};
use super::{
    Fns, MAX_TRANSCRIPT_BYTES, NATIVE_CLOSURE, NativeError, ORT, SHERPA, Sherpa, check_closure,
    only_expected_added, sample_count, stage, verify,
};
use crate::test_support::Scratch;

/// What the fake C functions do and saw, per test thread.
#[derive(Default)]
struct Fake {
    calls: Vec<&'static str>,
    accepted: Vec<(i32, Vec<f32>)>,
    config: Option<(String, i32, String, String, String)>,
    null_recognizer: bool,
    null_stream: bool,
    null_result: bool,
    /// The result's text, NUL added; `None` gives a null `text`.
    text: Option<Vec<u8>>,
}

thread_local! {
    static FAKE: RefCell<Fake> = RefCell::new(Fake::default());
}

fn fake<T>(f: impl FnOnce(&mut Fake) -> T) -> T {
    FAKE.with(|fake| f(&mut fake.borrow_mut()))
}

/// A result and the text it points into, freed together.
#[repr(C)]
struct FakeResult {
    result: RecognizerResult,
    text: Option<CString>,
}

fn read(text: *const c_char) -> String {
    // SAFETY: only called on strings the code under test set in a live config.
    unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned()
}

unsafe extern "C-unwind" fn create_recognizer(
    config: *const RecognizerConfig,
) -> *const RawRecognizer {
    // SAFETY: the code under test passes a live config.
    let config = unsafe { &*config };
    let model = &config.model_config;
    let seen = (
        read(model.transducer.encoder),
        model.num_threads,
        read(model.provider),
        read(model.model_type),
        read(config.decoding_method),
    );
    fake(|fake| {
        fake.calls.push("create recognizer");
        fake.config = Some(seen);
        if fake.null_recognizer {
            std::ptr::null()
        } else {
            Box::into_raw(Box::new(0_u8)).cast()
        }
    })
}

unsafe extern "C-unwind" fn destroy_recognizer(raw: *const RawRecognizer) {
    fake(|fake| fake.calls.push("destroy recognizer"));
    // SAFETY: made by create_recognizer's Box.
    drop(unsafe { Box::from_raw(raw.cast_mut().cast::<u8>()) });
}

unsafe extern "C-unwind" fn create_stream(_: *const RawRecognizer) -> *const RawStream {
    fake(|fake| {
        fake.calls.push("create stream");
        if fake.null_stream {
            std::ptr::null()
        } else {
            Box::into_raw(Box::new(0_u8)).cast()
        }
    })
}

unsafe extern "C-unwind" fn destroy_stream(raw: *const RawStream) {
    fake(|fake| fake.calls.push("destroy stream"));
    // SAFETY: made by create_stream's Box.
    drop(unsafe { Box::from_raw(raw.cast_mut().cast::<u8>()) });
}

unsafe extern "C-unwind" fn accept_waveform(
    _: *const RawStream,
    rate: i32,
    samples: *const f32,
    n: i32,
) {
    let n = usize::try_from(n).expect("a positive count");
    // SAFETY: the code under test passes `n` live samples.
    let samples = unsafe { std::slice::from_raw_parts(samples, n) }.to_vec();
    fake(|fake| {
        fake.calls.push("accept");
        fake.accepted.push((rate, samples));
    });
}

unsafe extern "C-unwind" fn decode_stream(_: *const RawRecognizer, _: *const RawStream) {
    fake(|fake| fake.calls.push("decode"));
}

unsafe extern "C-unwind" fn get_result(_: *const RawStream) -> *const RecognizerResult {
    let (null, text) = fake(|fake| {
        fake.calls.push("get result");
        (fake.null_result, fake.text.clone())
    });
    if null {
        return std::ptr::null();
    }
    let text = text.map(|text| CString::new(text).expect("fixture text has no NUL"));
    // SAFETY: all-zero is valid for a struct of integers, floats and raw pointers.
    let mut result: RecognizerResult = unsafe { std::mem::zeroed() };
    result.text = text.as_ref().map_or(std::ptr::null(), |text| text.as_ptr());
    Box::into_raw(Box::new(FakeResult { result, text })).cast()
}

unsafe extern "C-unwind" fn destroy_result(raw: *const RecognizerResult) {
    fake(|fake| fake.calls.push("destroy result"));
    // SAFETY: made by get_result's Box; `result` is FakeResult's first field.
    drop(unsafe { Box::from_raw(raw.cast_mut().cast::<FakeResult>()) });
}

unsafe extern "C-unwind" fn version() -> *const c_char {
    c"2.0.1".as_ptr()
}

fn sherpa(text: Option<&[u8]>) -> Sherpa {
    fake(|fake| {
        *fake = Fake {
            text: text.map(<[u8]>::to_vec),
            ..Fake::default()
        };
    });
    Sherpa::with_fns(Fns {
        create_recognizer,
        destroy_recognizer,
        create_stream,
        destroy_stream,
        accept_waveform,
        decode_stream,
        get_result,
        destroy_result,
        version,
        git_sha1: version,
    })
}

fn calls() -> Vec<&'static str> {
    fake(|fake| fake.calls.clone())
}

#[test]
fn a_decode_converts_once_and_drops_result_stream_then_recognizer() {
    let sherpa = sherpa(Some(b"hello world"));
    let recognizer = sherpa.recognizer(Path::new(r"C:\models")).expect("created");
    assert_eq!(
        recognizer.decode(44_100, &[0, 16_384, -32_768, i16::MAX]),
        Ok("hello world".to_owned())
    );
    drop(recognizer);
    assert_eq!(
        fake(|fake| fake.accepted.clone()),
        [(44_100, vec![0.0, 0.5, -1.0, 32_767.0 / 32_768.0])]
    );
    assert_eq!(
        calls(),
        [
            "create recognizer",
            "create stream",
            "accept",
            "decode",
            "get result",
            "destroy result",
            "destroy stream",
            "destroy recognizer",
        ]
    );
}

#[test]
fn the_recognizer_gets_parakeet_on_eight_cpu_threads() {
    let sherpa = sherpa(None);
    drop(sherpa.recognizer(Path::new(r"C:\models")).expect("created"));
    assert_eq!(
        fake(|fake| fake.config.clone()),
        Some((
            r"C:\models\encoder.int8.onnx".to_owned(),
            8,
            "cpu".to_owned(),
            "nemo_transducer".to_owned(),
            "greedy_search".to_owned(),
        ))
    );
}

#[test]
fn a_null_recognizer_is_create_and_nothing_is_destroyed() {
    let sherpa = sherpa(None);
    fake(|fake| fake.null_recognizer = true);
    assert_eq!(
        sherpa.recognizer(Path::new("models")).err(),
        Some(NativeError::Create)
    );
    assert_eq!(calls(), ["create recognizer"]);
}

#[test]
fn null_native_returns_fail_and_destroy_every_object_once() {
    type Case = (fn(&mut Fake), NativeError, &'static [&'static str]);
    let cases: [Case; 3] = [
        (
            |fake| fake.null_stream = true,
            NativeError::Create,
            &["create stream"],
        ),
        (
            |fake| fake.null_result = true,
            NativeError::Decode,
            &[
                "create stream",
                "accept",
                "decode",
                "get result",
                "destroy stream",
            ],
        ),
        (
            |fake| fake.text = None,
            NativeError::Decode,
            &[
                "create stream",
                "accept",
                "decode",
                "get result",
                "destroy result",
                "destroy stream",
            ],
        ),
    ];
    for (setup, error, expected) in cases {
        let sherpa = sherpa(Some(b"unused"));
        let recognizer = sherpa.recognizer(Path::new("models")).expect("created");
        fake(setup);
        assert_eq!(recognizer.decode(16_000, &[1]), Err(error));
        drop(recognizer);
        let mut want = vec!["create recognizer"];
        want.extend_from_slice(expected);
        want.push("destroy recognizer");
        assert_eq!(calls(), want, "{error:?}");
    }
}

#[test]
fn transcript_text_is_bounded_and_utf8_and_still_destroyed() {
    let at_limit = vec![b'a'; MAX_TRANSCRIPT_BYTES];
    let over = vec![b'a'; MAX_TRANSCRIPT_BYTES + 1];
    let cases: [(&[u8], Result<String, NativeError>); 4] = [
        (&at_limit, Ok("a".repeat(MAX_TRANSCRIPT_BYTES))),
        (&over, Err(NativeError::Text)),
        (b"caf\xc3", Err(NativeError::Text)),
        (b"", Ok(String::new())),
    ];
    for (text, want) in cases {
        let sherpa = sherpa(Some(text));
        let recognizer = sherpa.recognizer(Path::new("models")).expect("created");
        assert_eq!(recognizer.decode(16_000, &[1]), want);
        assert_eq!(
            calls()
                .iter()
                .filter(|call| call.starts_with("destroy"))
                .count(),
            2,
            "the result and the stream are destroyed"
        );
    }
}

#[test]
fn bad_input_is_refused_before_any_stream() {
    let sherpa = sherpa(Some(b"x"));
    let recognizer = sherpa.recognizer(Path::new("models")).expect("created");
    assert_eq!(recognizer.decode(0, &[1]), Err(NativeError::Input));
    assert_eq!(recognizer.decode(u32::MAX, &[1]), Err(NativeError::Input));
    assert_eq!(recognizer.decode(16_000, &[]), Err(NativeError::Input));
    assert_eq!(calls(), ["create recognizer"]);
    let max = usize::try_from(i32::MAX).expect("64-bit");
    assert_eq!(sample_count(max), Ok(i32::MAX));
    assert_eq!(sample_count(max + 1), Err(NativeError::Input));
}

fn scratch_file(scratch: &Scratch, bytes: &[u8]) -> PathBuf {
    std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
    let path = scratch.dir().join("pinned.dll");
    std::fs::write(&path, bytes).expect("scratch file");
    path
}

#[test]
fn a_native_is_checked_against_its_size_and_hash() {
    let scratch = Scratch::new("sherpa-pin");
    // SHA-256 of "abc".
    let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    let missing = scratch.dir().join("absent.dll");
    assert_eq!(verify(&missing, 3, abc).err(), Some(NativeError::Missing));
    let path = scratch_file(&scratch, b"abc");
    assert!(verify(&path, 3, abc).is_ok(), "the pinned bytes pass");
    assert_eq!(verify(&path, 4, abc).err(), Some(NativeError::Pin));
    let path = scratch_file(&scratch, b"abd");
    assert_eq!(verify(&path, 3, abc).err(), Some(NativeError::Pin));
}

#[test]
fn the_verified_handle_refuses_writers() {
    let scratch = Scratch::new("sherpa-hold");
    let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    let path = scratch_file(&scratch, b"abc");
    let held = verify(&path, 3, abc).expect("pinned");
    let write = std::fs::OpenOptions::new().write(true).open(&path);
    assert_eq!(
        write.map(drop).map_err(|error| error.raw_os_error()),
        Err(Some(32)),
        "ERROR_SHARING_VIOLATION while the handle is held"
    );
    drop(held);
}

// Staging and the held load path. The pins here are copies of a harmless system DLL under names
// no other test loads.

/// A fresh scratch folder in the local application data folder, removed on drop. The temp
/// folder won't do: on Rez it grants other accounts write access, which the checks refuse, and
/// on CI it's spelled with 8.3 short names.
struct TrustedScratch(PathBuf);

impl TrustedScratch {
    fn new(name: &str) -> Self {
        let dir = local_app_data().expect("local app data").join(format!(
            "{}-test-{}-{name}",
            kumochi_protocol::constants::OBJECT_PREFIX,
            std::process::id()
        ));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("old scratch folder removable");
        }
        std::fs::create_dir(&dir).expect("scratch folder");
        Self(dir)
    }
}

impl Drop for TrustedScratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn stage_root(name: &str) -> (TrustedScratch, PathBuf) {
    let scratch = TrustedScratch::new(name);
    let dir = scratch.0.clone();
    (scratch, dir)
}

fn fake_pin(source: &Path, name: &'static str) -> NativeFile {
    std::fs::create_dir_all(source).expect("source folder");
    let system = super::system_dir().expect("system folder");
    let bytes = std::fs::read(system.join("version.dll")).expect("version.dll");
    std::fs::write(source.join(name), &bytes).expect("pin written");
    let mut hex = String::new();
    for byte in Sha256::digest(&bytes) {
        write!(hex, "{byte:02x}").expect("a String takes writes");
    }
    NativeFile {
        name,
        bytes: u64::try_from(bytes.len()).expect("small"),
        sha256: Box::leak(hex.into_boxed_str()),
    }
}

fn staged_dir(root: &Path) -> PathBuf {
    root.join("natives").join("sherpa-onnx-2.0.1")
}

fn loaded_from(path: &Path) -> bool {
    let path = path.to_string_lossy().to_lowercase();
    super::modules()
        .expect("module list")
        .iter()
        .any(|module| module.to_string_lossy().to_lowercase() == path)
}

fn loaded_named(name: &str) -> bool {
    super::modules()
        .expect("module list")
        .iter()
        .any(|module| module.ends_with(name))
}

/// A protected DACL with one entry, which its files and folders inherit.
fn owner_only(folder: &Path) -> bool {
    let sddl = dacl_sddl(folder);
    let entries = sddl
        .strip_prefix("D:P")
        .map(|rest| rest.trim_start_matches("AI"));
    entries.is_some_and(|entries| {
        entries.matches('(').count() == 1 && entries.starts_with("(A;OICI;FA;;;")
    })
}

fn sharing_violation(result: io::Result<()>) -> bool {
    result.err().and_then(|error| error.raw_os_error()) == Some(32)
}

fn icacls(path: &Path, args: &[&str]) {
    run(std::process::Command::new("icacls").arg(path).args(args));
}

/// Makes the empty folder `dir` a junction to `target` in place: a mount-point reparse point set
/// through a handle opened for write-data and write-attributes, the rights NTFS asks for.
fn set_junction(dir: &Path, target: &Path) -> io::Result<()> {
    let folder = File::options()
        .access_mode(FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(dir)?;
    let substitute: Vec<u16> = format!(r"\??\{}", target.display())
        .encode_utf16()
        .collect();
    let name = u16::try_from(substitute.len() * 2).expect("a short path");
    let mut buffer = Vec::new();
    buffer.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    for field in [8 + name + 4, 0, 0, name, name + 2, 0] {
        buffer.extend(field.to_le_bytes());
    }
    for unit in substitute {
        buffer.extend(unit.to_le_bytes());
    }
    buffer.extend([0; 4]);
    let size = u32::try_from(buffer.len()).expect("small");
    let mut returned = 0;
    // SAFETY: `buffer` is a complete mount-point REPARSE_DATA_BUFFER of `size` bytes, the handle
    // is open, and no output buffer or overlapped I/O is used.
    let set = unsafe {
        DeviceIoControl(
            folder.as_raw_handle(),
            FSCTL_SET_REPARSE_POINT,
            buffer.as_ptr().cast(),
            size,
            std::ptr::null_mut(),
            0,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    if set == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[test]
fn final_paths_compare_by_drive_path_without_case() {
    assert!(final_path_matches(r"\\?\C:\Users\Me\x", r"c:\users\me\x\"));
    assert!(final_path_matches(r"\\?\C:\", r"C:\"));
    assert!(final_path_matches(r"\\?\C:\", r"C:"));
    assert!(!final_path_matches(r"\\?\D:\x", r"C:\x"));
    assert!(!final_path_matches(
        r"\\?\UNC\server\share\x",
        r"\\server\share\x"
    ));
    assert!(!final_path_matches(r"\\?\Volume{0}\x", r"C:\x"));
    assert!(
        !final_path_matches(r"C:\x", r"C:\x"),
        "always the long form"
    );
    assert!(!final_path_matches(
        r"\\?\C:\Users\RUNNER~1",
        r"C:\Users\runneradmin"
    ));
}

#[test]
fn staging_copies_checked_pins_into_owner_only_folders_and_keeps_them() {
    let (_scratch, root) = stage_root("stage");
    let source = root.join("source");
    let pin = fake_pin(&source, "kumochi-test-stage.dll");
    let staged = stage(&source, &root, std::slice::from_ref(&pin)).expect("staged");
    let file = staged_dir(&root).join(pin.name);
    assert_eq!(
        std::fs::read(&file).ok(),
        std::fs::read(source.join(pin.name)).ok()
    );
    assert!(owner_only(&root.join("natives")) && owner_only(&staged_dir(&root)));
    drop(staged);
    std::fs::remove_file(source.join(pin.name)).expect("source removed");
    drop(stage(&source, &root, std::slice::from_ref(&pin)).expect("a matching copy is kept"));
    std::fs::write(&file, b"drifted").expect("staged copy changed");
    assert_eq!(
        stage(&source, &root, std::slice::from_ref(&pin)).err(),
        Some(NativeError::Missing),
        "a drifted copy is replaced, which needs the source again"
    );
}

#[test]
fn a_source_that_misses_its_pin_leaves_nothing_staged() {
    let (_scratch, root) = stage_root("stage-bad");
    let source = root.join("source");
    let pin = fake_pin(&source, "kumochi-test-bad.dll");
    let wrong = NativeFile {
        sha256: "0000000000000000000000000000000000000000000000000000000000000000",
        ..pin
    };
    assert_eq!(
        stage(&source, &root, std::slice::from_ref(&wrong)).err(),
        Some(NativeError::Pin)
    );
    assert!(!staged_dir(&root).join(wrong.name).exists());
}

#[test]
fn nothing_rebinds_the_staged_pins_between_verification_and_load() {
    let (_scratch, root) = stage_root("stage-held");
    let source = root.join("source");
    let pin = fake_pin(&source, "kumochi-test-held.dll");
    let decoy = root.join("decoy");
    fake_pin(&decoy, "kumochi-test-held.dll");
    let staged = stage(&source, &root, std::slice::from_ref(&pin)).expect("staged");
    let dir = staged_dir(&root);
    let file = dir.join(pin.name);
    let away = root.join("away");
    assert!(sharing_violation(std::fs::rename(&file, &away)), "the DLL");
    assert!(sharing_violation(std::fs::remove_file(&file)), "the DLL");
    // A folder rename may fail on the open files below it first, so any refusal will do.
    for folder in [&dir, &root.join("natives"), &root] {
        let renamed = std::fs::rename(folder, root.with_extension("away"));
        assert!(renamed.is_err() && folder.is_dir(), "{renamed:?}");
    }
    assert!(
        set_junction(&dir, &decoy).is_err(),
        "a non-empty folder can't become a junction"
    );
    let library = staged.load(pin.name).expect("the verified bytes load");
    assert!(loaded_from(&file));
    assert!(!loaded_from(&decoy.join(pin.name)));
    drop(library);
}

#[test]
fn a_junction_on_the_root_path_is_refused_before_anything_loads() {
    let (_scratch, base) = stage_root("stage-junction");
    let real = base.join("real");
    let link = base.join("link");
    std::fs::create_dir_all(&real).expect("real folder");
    std::fs::create_dir_all(&link).expect("link folder");
    set_junction(&link, &real).expect("an empty folder becomes a junction");
    let source = base.join("source");
    let pin = fake_pin(&source, "kumochi-test-junction.dll");
    for root in [link.clone(), link.join("below")] {
        assert_eq!(
            stage(&source, &root, std::slice::from_ref(&pin)).err(),
            Some(NativeError::Reparse)
        );
    }
    std::fs::create_dir_all(staged_dir(&real)).expect("staging folders");
    let staged = staged_dir(&real).join("kumochi-test-junction.dll");
    std::fs::create_dir_all(&staged).expect("a folder where the DLL goes");
    set_junction(&staged, &source).expect("a junction where the DLL goes");
    assert_eq!(
        stage(&source, &real, std::slice::from_ref(&pin)).err(),
        Some(NativeError::Reparse),
        "a reparse point in the DLL's place"
    );
    assert!(!loaded_named(pin.name));
}

#[test]
fn an_existing_staging_folder_others_could_change_is_made_owner_only() {
    let (_scratch, root) = stage_root("stage-harden");
    let natives = root.join("natives");
    std::fs::create_dir_all(&natives).expect("natives folder");
    icacls(&natives, &["/grant", "*S-1-1-0:(F)"]);
    assert!(!owner_only(&natives));
    let source = root.join("source");
    let pin = fake_pin(&source, "kumochi-test-harden.dll");
    drop(stage(&source, &root, std::slice::from_ref(&pin)).expect("staged"));
    assert!(owner_only(&natives));
}

#[test]
fn a_root_others_can_change_is_refused() {
    let (_scratch, base) = stage_root("stage-open");
    let root = base.join("open");
    std::fs::create_dir_all(&root).expect("root folder");
    icacls(&root, &["/grant", "*S-1-1-0:(M)"]);
    let source = base.join("source");
    let pin = fake_pin(&source, "kumochi-test-open.dll");
    assert_eq!(
        stage(&source, &root, std::slice::from_ref(&pin)).err(),
        Some(NativeError::Unsafe)
    );
    assert!(!root.join("natives").exists());
}

#[test]
fn a_staging_folder_labelled_low_is_refused() {
    let (_scratch, root) = stage_root("stage-low-label");
    let natives = root.join("natives");
    std::fs::create_dir_all(&natives).expect("natives folder");
    icacls(&natives, &["/setintegritylevel", "low"]);
    let source = root.join("source");
    let pin = fake_pin(&source, "kumochi-test-label.dll");
    assert_eq!(
        stage(&source, &root, std::slice::from_ref(&pin)).err(),
        Some(NativeError::Unsafe)
    );
}

/// Set for the child process the low-integrity test starts: the staging root.
const LOW_CHILD: &str = "KUMOCHI_TEST_SHERPA_LOW";

/// A copy of this test binary labelled low runs at low integrity, as a sandboxed process under
/// this user would, and can't open anything on the held path with a right that changes it.
#[test]
fn a_lower_integrity_process_cant_change_the_staged_path() {
    let (_scratch, root) = stage_root("stage-low");
    let source = root.join("source");
    let pin = fake_pin(&source, "kumochi-test-low.dll");
    let staged = stage(&source, &root, std::slice::from_ref(&pin)).expect("staged");
    let exe = root.join("low.exe");
    std::fs::copy(std::env::current_exe().expect("own path"), &exe).expect("copied");
    icacls(&exe, &["/setintegritylevel", "low"]);
    let stdout = run(std::process::Command::new(&exe)
        .args([
            "--ignored",
            "--exact",
            "platform::sherpa::tests::child_at_low_integrity_changes_nothing",
        ])
        .env(LOW_CHILD, &root));
    assert!(stdout.contains("1 passed"), "the child passed: {stdout}");
    drop(staged);
}

#[test]
#[ignore = "the low-integrity test's child process"]
fn child_at_low_integrity_changes_nothing() {
    let Some(root) = std::env::var_os(LOW_CHILD).map(PathBuf::from) else {
        return;
    };
    assert!(
        std::fs::write(root.join("probe"), b"").is_err(),
        "it runs below medium"
    );
    let dir = staged_dir(&root);
    for path in [
        root.clone(),
        root.join("natives"),
        dir.clone(),
        dir.join("kumochi-test-low.dll"),
    ] {
        for access in [
            FILE_WRITE_ATTRIBUTES,
            FILE_WRITE_DATA,
            DELETE,
            WRITE_DAC,
            WRITE_OWNER,
        ] {
            let opened = File::options()
                .access_mode(access)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&path);
            assert!(opened.is_err(), "access {access:#x} was granted");
        }
    }
    assert!(set_junction(&dir, &root).is_err());
}

const SYSTEM: &str = r"C:\Windows\System32";
const NATIVES: &str = r"C:\Program Files\Kumochi";

fn closure(loaded: &[&str]) -> Result<(), NativeError> {
    let loaded: Vec<PathBuf> = loaded.iter().map(PathBuf::from).collect();
    check_closure(&loaded, Path::new(SYSTEM), Path::new(NATIVES))
}

#[test]
fn closure_modules_must_come_from_the_system_folder_or_the_pins() {
    assert_eq!(closure(&[r"C:\Windows\System32\VCRUNTIME140.dll"]), Ok(()));
    assert_eq!(closure(&[r"c:\windows\system32\vcruntime140.DLL"]), Ok(()));
    assert_eq!(
        closure(&[r"C:\Temp\x\VCRUNTIME140.dll"]),
        Err(NativeError::Conflict)
    );
    assert_eq!(
        closure(&[r"C:\Windows\System32evil\MSVCP140.dll"]),
        Err(NativeError::Conflict)
    );
    assert_eq!(
        closure(&[r"C:\Windows\System32\sub\MSVCP140.dll"]),
        Err(NativeError::Conflict)
    );
    assert_eq!(
        closure(&[r"C:\Program Files\Kumochi\onnxruntime.dll"]),
        Ok(())
    );
    assert_eq!(
        closure(&[r"C:\Program Files\KUMOCHI\Sherpa-Onnx-C-Api.dll"]),
        Ok(())
    );
    assert_eq!(
        closure(&[r"C:\Windows\System32\onnxruntime.dll"]),
        Err(NativeError::Conflict),
        "a pin anywhere else, even System32, is a conflict"
    );
    assert_eq!(
        closure(&[r"C:\Temp\other.dll", r"D:\app\kumochid.exe"]),
        Ok(()),
        "modules outside the closure don't matter"
    );
}

#[test]
fn only_pins_and_system_modules_may_appear_after_the_load() {
    let paths = |list: &[&str]| list.iter().map(PathBuf::from).collect::<Vec<_>>();
    let before = paths(&[r"D:\app\kumochid.exe", r"C:\Temp\already.dll"]);
    let check = |after: &[&str]| {
        only_expected_added(
            &before,
            &paths(after),
            Path::new(SYSTEM),
            Path::new(NATIVES),
        )
    };
    assert!(
        check(&[
            r"D:\app\kumochid.exe",
            r"C:\Temp\already.dll",
            r"C:\Program Files\Kumochi\onnxruntime.dll",
            r"C:\Program Files\Kumochi\sherpa-onnx-c-api.dll",
            r"C:\Windows\System32\dxgi.dll",
        ]),
        "pins and system modules"
    );
    assert!(
        !check(&[r"C:\Program Files\Kumochi\neighbor.dll"]),
        "a neighbor"
    );
    assert!(
        !check(&[r"C:\Windows\System32\sub\x.dll"]),
        "a system subfolder"
    );
}

/// Set for the child process the conflict test starts.
const CHILD: &str = "KUMOCHI_TEST_SHERPA_CHILD";

#[test]
fn a_preloaded_foreign_onnxruntime_is_a_conflict() {
    let scratch = Scratch::new("sherpa-conflict");
    std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
    let exe = std::env::current_exe().expect("own path");
    let stdout = run(std::process::Command::new(exe)
        .args([
            "--ignored",
            "--exact",
            "platform::sherpa::tests::child_preloads_a_foreign_onnxruntime",
        ])
        .env(CHILD, scratch.dir()));
    assert!(stdout.contains("1 passed"), "the child passed: {stdout}");
}

/// Runs only in the child process: loads a harmless system DLL renamed `onnxruntime.dll` from a
/// temp folder, then a load from another folder must refuse and load nothing.
#[test]
#[ignore = "the conflict test's child process"]
fn child_preloads_a_foreign_onnxruntime() {
    let Some(dir) = std::env::var_os(CHILD).map(PathBuf::from) else {
        return;
    };
    let system = super::system_dir().expect("system folder");
    let foreign = dir.join("onnxruntime.dll");
    std::fs::copy(system.join("version.dll"), &foreign).expect("copied");
    let _foreign = super::Library::load(&foreign).expect("the renamed copy loads");
    assert_eq!(
        Sherpa::load(&dir.join("natives"), &dir).err(),
        Some(NativeError::Conflict)
    );
    let modules = super::modules().expect("module list");
    assert!(
        !modules
            .iter()
            .any(|path| path.ends_with("sherpa-onnx-c-api.dll")),
        "sherpa-onnx never loaded"
    );
}

// Rez only, ignored in CI. They read the real DLLs from `SHERPA_NATIVES` (default: beside the
// test binary's profile folder, where `cargo xtask voice-assets link` puts them), the header from
// `SHERPA_INCLUDE`, the models from `VOICE_MODELS` and the clips from `STT_CLIPS`.

fn env_dir(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("set {name}")))
}

fn natives_dir() -> PathBuf {
    std::env::var_os("SHERPA_NATIVES").map_or_else(
        || {
            let exe = std::env::current_exe().expect("own path");
            exe.ancestors()
                .nth(2)
                .expect("target/<profile>/deps")
                .to_owned()
        },
        PathBuf::from,
    )
}

/// Runs `command` to the end and returns its stdout; it must succeed. Only the start holds
/// [`crate::platform::spawn_lock`], since handles are inherited only then.
#[expect(clippy::disallowed_methods, reason = "spawns while holding spawn_lock")]
fn run(command: &mut std::process::Command) -> String {
    let child = {
        let _spawning = crate::platform::spawn_lock();
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("the tool starts")
    };
    let output = child.wait_with_output().expect("the tool runs");
    assert!(output.status.success(), "{command:?} failed: {output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Compiles a probe against the real `c-api.h` with `cl` and checks every `LAYOUT` value.
fn check_layout_against_the_header() {
    let scratch = Scratch::new("sherpa-layout");
    std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
    let mut probe = String::from(
        "#include <stdio.h>\n#include <stddef.h>\n#include <stdalign.h>\n\
         #include \"sherpa-onnx/c-api/c-api.h\"\nint main(void) {\n",
    );
    for (name, _) in LAYOUT {
        writeln!(probe, "  printf(\"%zu\\n\", (size_t)({name}));").expect("a String takes writes");
    }
    probe.push_str("  return 0;\n}\n");
    std::fs::write(scratch.dir().join("probe.c"), probe).expect("probe written");
    let include = env_dir("SHERPA_INCLUDE");
    run(std::process::Command::new("cl")
        .current_dir(scratch.dir())
        .args(["/nologo", "/std:c11", "/I"])
        .arg(&include)
        .args(["probe.c", "/Fe:probe.exe"]));
    let printed = run(&mut std::process::Command::new(
        scratch.dir().join("probe.exe"),
    ));
    let from_c: Vec<usize> = printed
        .lines()
        .map(|line| line.trim().parse().expect("a number"))
        .collect();
    assert_eq!(from_c.len(), LAYOUT.len(), "one line per value");
    for ((name, rust), c) in LAYOUT.iter().zip(from_c) {
        assert_eq!(*rust, c, "{name}");
    }
}

#[test]
#[ignore = "Rez only: needs cl and c-api.h"]
fn measure_sherpa_layout() {
    check_layout_against_the_header();
}

/// The closure list must cover every DLL the two pins import, other than API sets and `KnownDLLs`.
#[test]
#[ignore = "Rez only: needs dumpbin and the pinned DLLs"]
fn measure_native_imports() {
    let natives = natives_dir();
    let mut imports = std::collections::BTreeSet::new();
    for dll in [SHERPA, ORT] {
        let listing = run(std::process::Command::new("dumpbin")
            .arg("/dependents")
            .arg(natives.join(dll)));
        imports.extend(
            listing
                .lines()
                .map(|line| line.trim().to_lowercase())
                .filter(|line| {
                    !line.contains(' ')
                        && Path::new(line)
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("dll"))
                })
                .filter(|name| !name.starts_with("api-ms-win-") && name != dll),
        );
    }
    let known = run(std::process::Command::new("reg").args([
        "query",
        r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\KnownDLLs",
    ]));
    let known: std::collections::BTreeSet<String> = known
        .lines()
        .filter_map(|line| line.split_whitespace().last())
        .map(str::to_lowercase)
        .collect();
    for name in &imports {
        assert!(
            NATIVE_CLOSURE.contains(&name.as_str()) || known.contains(name),
            "{name} is imported but not in NATIVE_CLOSURE"
        );
    }
    for name in NATIVE_CLOSURE {
        assert!(
            imports.contains(name) || [SHERPA, "ucrtbase.dll"].contains(&name),
            "{name} is in NATIVE_CLOSURE but nothing imports it"
        );
    }
}

/// A DLL planted beside the pins under one of their imports' names is never loaded.
#[test]
#[ignore = "Rez only: loads the pinned DLLs into this process"]
fn measure_planted_neighbor() {
    let (_scratch, root) = stage_root("sherpa-neighbor");
    drop(stage(&natives_dir(), &root, kumochi_protocol::voice::NATIVE_FILES).expect("staged"));
    let system = super::system_dir().expect("system folder");
    std::fs::copy(
        system.join("version.dll"),
        staged_dir(&root).join("MSVCP140.dll"),
    )
    .expect("neighbor planted beside the staged pins");
    let sherpa = Sherpa::load(&natives_dir(), &root).expect("loads with the neighbor ignored");
    let modules = super::modules().expect("module list");
    let dir = staged_dir(&root).to_string_lossy().to_lowercase();
    let from_scratch: Vec<String> = modules
        .iter()
        .map(|path| path.to_string_lossy().to_lowercase())
        .filter(|path| path.starts_with(&dir))
        .collect();
    assert_eq!(from_scratch.len(), 2, "only the pins: {from_scratch:?}");
    let msvcp = modules
        .iter()
        .find(|path| path.ends_with("MSVCP140.dll") || path.ends_with("msvcp140.dll"))
        .expect("sherpa imports MSVCP140");
    assert_eq!(
        msvcp
            .parent()
            .map(|dir| dir.to_string_lossy().to_lowercase()),
        Some(system.to_string_lossy().to_lowercase())
    );
    drop(sherpa);
}
