//! `cargo xtask voice-assets link <models> <natives>`: checks the voice files already on a dev
//! machine against the manifest and copies them where a dev build looks, so nothing downloads
//! twice. It never downloads.

use std::path::{Path, PathBuf};
use std::{fs, io};

use anyhow::{Context as _, Result, bail, ensure};
use kumochi_protocol::constants::DATA_DIR;
use kumochi_protocol::voice::{MODEL_FILES, MODELS_DIR, ModelFile, NATIVE_FILES, VoicePart};

use crate::platform::gate_capture::file_sha256;

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
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .context("%LOCALAPPDATA% is not set to an absolute path")?;
    Ok(local.join(DATA_DIR).join(MODELS_DIR))
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

/// Keeps every destination that already verifies and copies the absent ones. Any other existing
/// destination is a conflict: it's reported and left as it is, and nothing is copied. If a copy
/// fails partway, the files this run made are removed again.
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
    let mut made: Vec<&Path> = Vec::with_capacity(missing.len());
    for placement in missing {
        if let Err(error) = copy_new(placement, &mut made) {
            let stranded: Vec<String> = made
                .into_iter()
                .filter(|dest| fs::remove_file(dest).is_err())
                .map(|dest| dest.display().to_string())
                .collect();
            if stranded.is_empty() {
                return Err(error);
            }
            return Err(error.context(format!(
                "these files from this run couldn't be removed: {}",
                stranded.join(", ")
            )));
        }
    }
    Ok(())
}

/// Copies a source to a new destination, which takes its folder's permissions. A hardlink would
/// share the source's, which the brain refuses where other accounts may write. It never replaces
/// a destination, and one it creates joins `made` at once, so a failed copy is removed too.
fn copy_new<'a>(placement: &'a Placement<'_>, made: &mut Vec<&'a Path>) -> Result<()> {
    let dest = placement.dest.as_path();
    let parent = dest.parent().context("a destination has a folder")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let mut file =
        fs::File::create_new(dest).with_context(|| format!("creating {}", dest.display()))?;
    made.push(dest);
    let mut source = fs::File::open(&placement.source)
        .with_context(|| format!("opening {}", placement.source.display()))?;
    io::copy(&mut source, &mut file).with_context(|| {
        format!(
            "copying {} to {}",
            placement.source.display(),
            dest.display()
        )
    })?;
    verify(dest, placement.bytes, placement.sha256)
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

    use super::{Placement, S5_LAYOUT, install, link, s5_path, verify};
    use crate::scratch::Scratch;

    fn scratch(name: &str) -> Scratch {
        crate::scratch::dir(&format!("voice-assets-{name}"))
    }

    fn voice_sha() -> String {
        let dir = scratch("sha");
        let path = dir.join("f");
        fs::write(&path, b"voice").expect("write");
        crate::platform::gate_capture::file_sha256(&path).expect("hash")
    }

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
    }

    #[test]
    fn verify_accepts_only_the_exact_size_and_hash() {
        let dir = scratch("verify");
        let path = dir.join("f");
        fs::write(&path, b"voice").expect("write");
        let sha = voice_sha();
        verify(&path, 5, &sha).expect("exact size and hash");
        let short = verify(&path, 4, &sha).expect_err("one byte short");
        assert!(
            short
                .to_string()
                .contains("is 5 bytes; the manifest pins 4"),
            "{short}"
        );
        let long = verify(&path, 6, &sha).expect_err("one byte over");
        assert!(
            long.to_string().contains("is 5 bytes; the manifest pins 6"),
            "{long}"
        );
        let wrong = verify(&path, 5, &"0".repeat(64)).expect_err("wrong hash");
        assert!(wrong.to_string().contains("has SHA-256"), "{wrong}");
        let missing = verify(&dir.join("absent"), 5, &sha).expect_err("missing");
        assert!(missing.to_string().contains("is missing"), "{missing}");
        let folder = verify(&dir, 0, &sha).expect_err("a folder");
        assert!(
            folder.to_string().contains("not a regular file"),
            "{folder}"
        );
    }

    #[test]
    fn install_copies_and_keeps_a_verified_destination() {
        let dir = scratch("install");
        let source = dir.join("source");
        fs::write(&source, b"voice").expect("write");
        let sha = voice_sha();
        let dest = dir.join("models").join("part").join("f");
        install(&[placement(&source, &dest, &sha)]).expect("copies into a new folder");
        assert_eq!(fs::read(&dest).expect("read"), b"voice");

        fs::write(&source, b"VOICE").expect("rewrite the source in place");
        assert_eq!(
            fs::read(&dest).expect("read"),
            b"voice",
            "the destination is a copy with its folder's permissions, not the source's file"
        );
        fs::write(&source, b"voice").expect("restore");

        install(&[placement(&dir.join("absent"), &dest, &sha)])
            .expect("a verified destination is left alone");
    }

    #[test]
    fn a_conflicting_destination_changes_nothing() {
        let dir = scratch("conflict");
        let source = dir.join("source");
        fs::write(&source, b"voice").expect("write");
        let sha = voice_sha();
        let fresh = dir.join("models").join("a");
        let conflicting = dir.join("models").join("b");
        fs::create_dir_all(dir.join("models")).expect("models folder");
        fs::write(&conflicting, b"wrong").expect("write");

        let error = install(&[
            placement(&source, &fresh, &sha),
            placement(&source, &conflicting, &sha),
        ])
        .expect_err("a mismatched destination fails");
        assert!(error.to_string().contains("nothing was copied"), "{error}");
        assert_eq!(
            fs::read(&conflicting).expect("read"),
            b"wrong",
            "the conflicting bytes survive"
        );
        assert!(!fresh.exists(), "nothing was copied before the conflict");
    }

    #[test]
    fn a_failed_copy_removes_the_files_this_run_made() {
        let dir = scratch("rollback");
        let source = dir.join("source");
        fs::write(&source, b"voice").expect("write");
        let sha = voice_sha();
        let first = dir.join("models").join("a");
        let second = dir.join("models").join("b");

        install(&[
            placement(&source, &first, &sha),
            placement(&dir.join("absent"), &second, &sha),
        ])
        .expect_err("the second copy has no source");
        assert!(!first.exists(), "the first copy was removed again");
        assert!(!second.exists(), "the second copy's new file was removed");
        assert_eq!(fs::read(&source).expect("read"), b"voice");
    }

    #[test]
    fn a_bad_source_links_nothing() {
        let dir = scratch("link");
        let models_root = dir.join("dest");
        let error = link(
            &dir.join("models"),
            &dir.join("natives"),
            &models_root,
            &[dir.join("bin")],
        )
        .expect_err("no sources");
        assert!(error.to_string().contains("is missing"), "{error}");
        assert!(!models_root.exists(), "nothing was created");
        assert!(!dir.join("bin").exists(), "nothing was created");
    }
}
