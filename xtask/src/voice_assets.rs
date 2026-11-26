//! `cargo xtask voice-assets link <models> <natives>`: checks the voice files already on a dev
//! machine against the manifest and copies them where a dev build looks, so nothing downloads
//! twice. It never downloads.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail, ensure};
use kumochi_protocol::constants::{
    DATA_DIR, HONOURS_APPDATA_OVERRIDE, LOCAL_APPDATA_OVERRIDE, ROAMING_APPDATA_OVERRIDE,
};
use kumochi_protocol::voice::{MODEL_FILES, MODELS_DIR, ModelFile, NATIVE_FILES, VoicePart};

use crate::platform::gate_capture::{file_sha256, sha256_through};

const USAGE: &str = "usage: cargo xtask voice-assets link <models-dir> <natives-dir>";

/// Where each model file sits in S5's `models` folder on Rez, by part and manifest name.
const S5_LAYOUT: &[(VoicePart, &str, &str)] = &[
    (
        VoicePart::SpeechInput,
        "encoder.int8.onnx",
        "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/encoder.int8.onnx",
    ),
    (
        VoicePart::SpeechInput,
        "decoder.int8.onnx",
        "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/decoder.int8.onnx",
    ),
    (
        VoicePart::SpeechInput,
        "joiner.int8.onnx",
        "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/joiner.int8.onnx",
    ),
    (
        VoicePart::SpeechInput,
        "tokens.txt",
        "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/tokens.txt",
    ),
    (
        VoicePart::SpokenReplies,
        "model.q8.gguf",
        "phonon31m/model.q8.gguf",
    ),
    (
        VoicePart::SpokenReplies,
        "config.json",
        "phonon31m/config.json",
    ),
    (
        VoicePart::SpokenReplies,
        "tokenizer.json",
        "phonon31m/tokenizer.json",
    ),
    (
        VoicePart::SpokenReplies,
        "juniper.safetensors",
        "phonon31m/voices/Juniper.safetensors",
    ),
];

pub(crate) fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    let (Some("link"), Some(models), Some(natives), None) = (
        args.next().as_deref(),
        args.next(),
        args.next(),
        args.next(),
    ) else {
        bail!(USAGE);
    };
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask sits inside the workspace")?;
    let bins = [
        workspace.join("target").join("debug"),
        workspace.join("target").join("release"),
    ];
    let linked = link(
        Path::new(&models),
        Path::new(&natives),
        &models_root()?,
        &bins,
    )?;
    for path in linked {
        println!("{}", path.display());
    }
    Ok(())
}

// shortcut: links for the xtask's own build profile (debug: "Kumochi Dev"), so a release brain
// looks under "Kumochi"; add a profile flag when a release measurement needs linked models.
fn models_root() -> Result<PathBuf> {
    if HONOURS_APPDATA_OVERRIDE {
        refuse_override(|name| std::env::var_os(name))?;
    }
    let local = kumochi_storage::local_app_data(kumochi_storage::Today::Environment)
        .context("resolving the local application data folder the brain uses")?;
    Ok(local.join(DATA_DIR).join(MODELS_DIR))
}

/// Refuses while either half of the folder override is set, in a build that honours it: the brain
/// admits its models folder under an override profile at start, and this copy doesn't, so it links
/// only into the everyday dev folder.
fn refuse_override(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<()> {
    ensure!(
        var(LOCAL_APPDATA_OVERRIDE).is_none() && var(ROAMING_APPDATA_OVERRIDE).is_none(),
        "voice-assets links only into the everyday dev folder: unset {LOCAL_APPDATA_OVERRIDE} and \
         {ROAMING_APPDATA_OVERRIDE}"
    );
    Ok(())
}

fn s5_path(file: &ModelFile) -> Result<&'static str> {
    S5_LAYOUT
        .iter()
        .find(|(part, name, _)| *part == file.part && *name == file.name)
        .map(|(_, _, path)| *path)
        .with_context(|| format!("no S5 path for {}", file.name))
}

/// One destination to fill from a verified source.
struct Placement<'a> {
    source: PathBuf,
    dest: PathBuf,
    bytes: u64,
    sha256: &'a str,
}

/// Verifies every source, then every existing destination, before copying anything, so a bad
/// source or a conflicting destination changes nothing. Returns the destinations.
fn link(
    models: &Path,
    natives: &Path,
    models_root: &Path,
    bins: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    let mut plan = Vec::new();
    for file in MODEL_FILES {
        let source = models.join(s5_path(file)?);
        verify(&source, file.bytes, file.sha256)?;
        plan.push(Placement {
            source,
            dest: models_root.join(file.part.dir()).join(file.name),
            bytes: file.bytes,
            sha256: file.sha256,
        });
    }
    for file in NATIVE_FILES {
        let source = natives.join(file.name);
        verify(&source, file.bytes, file.sha256)?;
        for bin in bins {
            plan.push(Placement {
                source: source.clone(),
                dest: bin.join(file.name),
                bytes: file.bytes,
                sha256: file.sha256,
            });
        }
    }
    install(&plan)?;
    Ok(plan.into_iter().map(|placement| placement.dest).collect())
}

/// Copies speech input's model files from an installed models folder, laid out as
/// `<models>/speech-input/<file>`, into `models_root`, and the native runtime from `natives` into
/// `bin`. Every source is verified against the manifest first, as in [`link`].
pub(crate) fn place_speech_input(
    models: &Path,
    natives: &Path,
    models_root: &Path,
    bin: &Path,
) -> Result<()> {
    let mut plan = Vec::new();
    for file in MODEL_FILES
        .iter()
        .filter(|file| file.part == VoicePart::SpeechInput)
    {
        let source = models.join(file.part.dir()).join(file.name);
        verify(&source, file.bytes, file.sha256)?;
        plan.push(Placement {
            source,
            dest: models_root.join(file.part.dir()).join(file.name),
            bytes: file.bytes,
            sha256: file.sha256,
        });
    }
    for file in NATIVE_FILES {
        let source = natives.join(file.name);
        verify(&source, file.bytes, file.sha256)?;
        plan.push(Placement {
            source,
            dest: bin.join(file.name),
            bytes: file.bytes,
            sha256: file.sha256,
        });
    }
    install(&plan)
}

/// Keeps every destination that already verifies and copies the absent ones. Any other existing
/// destination is a conflict: it's reported and left as it is, and nothing is copied. A failed copy
/// leaves nothing at its final name; files this run already placed stay, since each verified.
fn install(plan: &[Placement<'_>]) -> Result<()> {
    let mut missing = Vec::new();
    let mut conflicts = Vec::new();
    for placement in plan {
        if fs::symlink_metadata(&placement.dest).is_err() {
            missing.push(placement);
        } else if let Err(error) = verify(&placement.dest, placement.bytes, placement.sha256) {
            conflicts.push(format!("{error:#}"));
        }
    }
    ensure!(
        conflicts.is_empty(),
        "nothing was copied, because existing files don't match the manifest; move them aside \
         and run this again:\n{}",
        conflicts.join("\n")
    );
    missing.into_iter().try_for_each(publish)
}

// shortcut: no junction, symlink or file-identity checks on the destination folders, which are the
// developer's own %LOCALAPPDATA%, target folders and a gate run's own folders; add them if this
// ever writes elsewhere.
/// Copies the source to a temporary file beside the destination, verifying the bytes as they are
/// written, then links the final name to it. A hardlink to the source would share its ACL, which
/// the brain refuses where other accounts may write.
fn publish(placement: &Placement<'_>) -> Result<()> {
    let dest = placement.dest.as_path();
    let parent = dest.parent().context("a destination has a folder")?;
    let name = dest
        .file_name()
        .context("a destination has a file name")?
        .to_string_lossy();
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let temp = parent.join(format!("{name}.voice-assets-{}.tmp", std::process::id()));
    let file =
        fs::File::create_new(&temp).with_context(|| format!("creating {}", temp.display()))?;
    let published = copy_verified(placement, file).and_then(|()| {
        // Our own copy has the destination folder's ACL, so the brain accepts the link.
        fs::hard_link(&temp, dest).with_context(|| format!("placing {}", dest.display()))
    });
    let removed = fs::remove_file(&temp).with_context(|| format!("removing {}", temp.display()));
    published.and(removed)
}

/// Copies the source into `file` and checks the written size and hash against the manifest.
fn copy_verified(placement: &Placement<'_>, mut file: fs::File) -> Result<()> {
    let source = fs::File::open(&placement.source)
        .with_context(|| format!("opening {}", placement.source.display()))?;
    let sha256 = sha256_through(source, &mut file)
        .with_context(|| format!("copying {}", placement.source.display()))?;
    let bytes = file.metadata()?.len();
    ensure!(
        bytes == placement.bytes,
        "{} copied {bytes} bytes; the manifest pins {}",
        placement.source.display(),
        placement.bytes
    );
    ensure!(
        sha256 == placement.sha256,
        "{} copied with SHA-256 {sha256}; the manifest pins {}",
        placement.source.display(),
        placement.sha256
    );
    Ok(())
}

fn verify(path: &Path, bytes: u64, sha256: &str) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("{} is missing", path.display()))?;
    ensure!(
        metadata.is_file(),
        "{} is not a regular file",
        path.display()
    );
    let len = metadata.len();
    ensure!(
        len == bytes,
        "{} is {len} bytes; the manifest pins {bytes}",
        path.display()
    );
    let actual = file_sha256(path)?;
    ensure!(
        actual == sha256,
        "{} has SHA-256 {actual}; the manifest pins {sha256}",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use kumochi_protocol::voice::MODEL_FILES;

    use super::{Placement, S5_LAYOUT, install, link, publish, refuse_override, s5_path, verify};
    use crate::scratch::Scratch;

    #[test]
    fn a_debug_xtask_refuses_to_link_under_any_half_of_the_override() {
        let refusal = "voice-assets links only into the everyday dev folder: unset \
                       KUMOCHI_GATE_LOCALAPPDATA and KUMOCHI_GATE_APPDATA";
        for set in [
            &["KUMOCHI_GATE_LOCALAPPDATA", "KUMOCHI_GATE_APPDATA"][..],
            &["KUMOCHI_GATE_LOCALAPPDATA"],
            &["KUMOCHI_GATE_APPDATA"],
        ] {
            let var = |name: &str| set.contains(&name).then(|| r"C:\profile".into());
            let error = refuse_override(var).expect_err("an override is set");
            assert_eq!(error.to_string(), refusal, "{set:?}");
        }
        refuse_override(|_| None).expect("no override");
    }

    fn scratch(name: &str) -> Scratch {
        crate::scratch::dir(&format!("voice-assets-{name}"))
    }

    const VOICE_SHA: &str = "c57d7e92019708b614c90fa3685cd644f543a60153fb99ec9b67c381a245fb2a";

    fn placement<'a>(source: &Path, dest: &Path, sha: &'a str) -> Placement<'a> {
        Placement {
            source: source.to_path_buf(),
            dest: dest.to_path_buf(),
            bytes: 5,
            sha256: sha,
        }
    }

    #[test]
    fn every_manifest_file_has_exactly_one_s5_path() {
        for file in MODEL_FILES {
            let matches = S5_LAYOUT
                .iter()
                .filter(|(part, name, _)| *part == file.part && *name == file.name)
                .count();
            assert_eq!(matches, 1, "{}", file.name);
            assert!(s5_path(file).is_ok(), "{}", file.name);
        }
        assert_eq!(S5_LAYOUT.len(), MODEL_FILES.len(), "no stray S5 entries");
        let juniper = MODEL_FILES
            .iter()
            .find(|file| file.name == "juniper.safetensors")
            .expect("Juniper is listed");
        assert_eq!(
            s5_path(juniper).expect("S5 path"),
            "phonon31m/voices/Juniper.safetensors"
        );
    }

    #[test]
    fn verify_accepts_only_the_exact_size_and_hash() {
        let dir = scratch("verify");
        let path = dir.join("f");
        fs::write(&path, b"voice").expect("write");
        verify(&path, 5, VOICE_SHA).expect("exact size and hash");
        for bytes in [4, 6] {
            assert!(verify(&path, bytes, VOICE_SHA).is_err(), "{bytes} bytes");
        }
        assert!(verify(&path, 5, &"0".repeat(64)).is_err(), "wrong digest");
        assert!(
            verify(&dir.join("absent"), 5, VOICE_SHA).is_err(),
            "missing file"
        );
        assert!(verify(&dir, 0, VOICE_SHA).is_err(), "directory");
    }

    #[test]
    fn install_copies_and_keeps_a_verified_destination() {
        let dir = scratch("install");
        let source = dir.join("source");
        fs::write(&source, b"voice").expect("write");
        let dest = dir.join("models").join("part").join("f");
        install(&[placement(&source, &dest, VOICE_SHA)]).expect("copies into a new folder");
        assert_eq!(fs::read(&dest).expect("read"), b"voice");

        fs::write(&source, b"VOICE").expect("rewrite the source in place");
        assert_eq!(
            fs::read(&dest).expect("read"),
            b"voice",
            "the destination is a copy with its folder's permissions, not the source's file"
        );
        fs::write(&source, b"voice").expect("restore");

        install(&[placement(&dir.join("absent"), &dest, VOICE_SHA)])
            .expect("a verified destination is left alone");
    }

    #[test]
    fn a_conflicting_destination_changes_nothing() {
        let dir = scratch("conflict");
        let source = dir.join("source");
        fs::write(&source, b"voice").expect("write");
        let fresh = dir.join("models").join("a");
        let conflicting = dir.join("models").join("b");
        fs::create_dir_all(dir.join("models")).expect("models folder");
        fs::write(&conflicting, b"wrong").expect("write");

        let error = install(&[
            placement(&source, &fresh, VOICE_SHA),
            placement(&source, &conflicting, VOICE_SHA),
        ])
        .expect_err("a mismatched destination fails");
        assert!(
            error
                .to_string()
                .contains(&conflicting.display().to_string()),
            "{error}"
        );
        assert_eq!(
            fs::read(&conflicting).expect("read"),
            b"wrong",
            "the conflicting bytes survive"
        );
        assert!(!fresh.exists(), "nothing was copied before the conflict");
    }

    fn temporaries(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .expect("list")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.contains(".voice-assets-"))
            .collect()
    }

    #[test]
    fn a_failed_copy_keeps_the_placed_files_and_leaves_no_temporary() {
        let dir = scratch("rollback");
        let source = dir.join("source");
        fs::write(&source, b"voice").expect("write");
        let first = dir.join("models").join("a");
        let second = dir.join("models").join("b");

        install(&[
            placement(&source, &first, VOICE_SHA),
            placement(&dir.join("absent"), &second, VOICE_SHA),
        ])
        .expect_err("the second copy has no source");
        assert_eq!(
            fs::read(&first).expect("read"),
            b"voice",
            "a placed file stays"
        );
        assert!(!second.exists(), "nothing at the failed copy's final name");
        assert_eq!(temporaries(&dir.join("models")), Vec::<String>::new());
        assert_eq!(fs::read(&source).expect("read"), b"voice");
    }

    #[test]
    fn a_wrong_hash_copy_places_nothing() {
        let dir = scratch("wrong-hash");
        let source = dir.join("source");
        fs::write(&source, b"voice").expect("write");
        let models = dir.join("models");
        let dest = models.join("f");

        let error = install(&[placement(&source, &dest, &"0".repeat(64))])
            .expect_err("the copy's hash doesn't match");
        assert!(
            error.to_string().contains("copied with SHA-256"),
            "{error:#}"
        );
        assert!(!dest.exists(), "no final file");
        assert_eq!(temporaries(&models), Vec::<String>::new());
    }

    #[test]
    fn an_existing_final_file_is_never_replaced() {
        let dir = scratch("existing");
        let source = dir.join("source");
        fs::write(&source, b"voice").expect("write");
        let models = dir.join("models");
        fs::create_dir_all(&models).expect("models folder");
        let dest = models.join("f");
        fs::write(&dest, b"other").expect("a file arrives at the final name");

        publish(&placement(&source, &dest, VOICE_SHA)).expect_err("the final name exists");
        assert_eq!(fs::read(&dest).expect("read"), b"other", "left as it was");
        assert_eq!(temporaries(&models), Vec::<String>::new());
    }

    #[test]
    fn a_bad_source_links_nothing() {
        let dir = scratch("link");
        let models = dir.join("models");
        let natives = dir.join("natives");
        let models_root = dir.join("dest");
        let error =
            link(&models, &natives, &models_root, &[dir.join("bin")]).expect_err("no sources");
        let diagnostic = error.to_string();
        assert!(
            diagnostic.contains(&models.display().to_string())
                || diagnostic.contains(&natives.display().to_string()),
            "{diagnostic}"
        );
        assert!(!models_root.exists(), "nothing was created");
        assert!(!dir.join("bin").exists(), "nothing was created");
    }
}
