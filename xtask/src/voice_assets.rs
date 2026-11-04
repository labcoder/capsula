//! `cargo xtask voice-assets link <models> <natives>`: checks the voice files already on a dev
//! machine against the manifest and hardlinks them where a dev build looks, so nothing downloads
//! twice. It never copies and never downloads.

use std::fs;
use std::path::{Path, PathBuf};

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

/// Verifies every source before linking anything, so a bad source changes nothing. Returns the
/// linked paths.
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
        let dest = models_root.join(file.part.dir()).join(file.name);
        plan.push((source, dest, file.bytes, file.sha256));
    }
    for file in NATIVE_FILES {
        let source = natives.join(file.name);
        verify(&source, file.bytes, file.sha256)?;
        for bin in bins {
            plan.push((source.clone(), bin.join(file.name), file.bytes, file.sha256));
        }
    }
    let mut linked = Vec::with_capacity(plan.len());
    for (source, dest, bytes, sha256) in plan {
        place(&source, &dest, bytes, sha256)?;
        linked.push(dest);
    }
    Ok(linked)
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

/// Leaves a destination that already verifies, and replaces any other with a hardlink.
fn place(source: &Path, dest: &Path, bytes: u64, sha256: &str) -> Result<()> {
    if fs::symlink_metadata(dest).is_ok() {
        if verify(dest, bytes, sha256).is_ok() {
            return Ok(());
        }
        fs::remove_file(dest).with_context(|| format!("removing stale {}", dest.display()))?;
    }
    let parent = dest.parent().context("a destination has a folder")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    fs::hard_link(source, dest).with_context(|| {
        format!(
            "hardlinking {} to {}: both must be on one NTFS volume, and nothing is copied",
            source.display(),
            dest.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use kumochi_protocol::voice::MODEL_FILES;

    use super::{S5_LAYOUT, link, place, s5_path, verify};

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "voice-assets-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("the clock is after 1970")
                .as_nanos()
        ));
        fs::create_dir_all(&path).expect("scratch folder");
        path
    }

    fn voice_sha() -> String {
        let dir = scratch("sha");
        let path = dir.join("f");
        fs::write(&path, b"voice").expect("write");
        let sha = crate::platform::gate_capture::file_sha256(&path).expect("hash");
        fs::remove_dir_all(dir).expect("cleanup");
        sha
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
        fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn place_links_keeps_a_good_copy_and_replaces_a_bad_one() {
        let dir = scratch("place");
        let source = dir.join("source");
        fs::write(&source, b"voice").expect("write");
        let sha = voice_sha();
        let dest = dir.join("models").join("part").join("f");
        place(&source, &dest, 5, &sha).expect("links into a new folder");
        assert_eq!(fs::read(&dest).expect("read"), b"voice");
        place(&dir.join("absent"), &dest, 5, &sha).expect("a verified copy is left alone");

        let stale = dir.join("models").join("part").join("g");
        fs::write(&stale, b"wrong").expect("write");
        place(&source, &stale, 5, &sha).expect("replaces a mismatched copy");
        assert_eq!(fs::read(&stale).expect("read"), b"voice");
        fs::remove_dir_all(dir).expect("cleanup");
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
        fs::remove_dir_all(dir).expect("cleanup");
    }
}
