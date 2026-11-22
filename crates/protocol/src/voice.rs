//! The voice files Kumochi installs: each model file and native library, where it comes from, and
//! the size and SHA-256 it must have. The brain downloads and checks against this table, and
//! `cargo xtask voice-assets` links dev copies from it; nothing else lists these files.

use serde::{Deserialize, Serialize};

/// One of the two parts a user installs separately. Append only: the tags travel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VoicePart {
    /// Parakeet TDT 0.6B v3 (int8), speech to text.
    SpeechInput,
    /// Phonon 31M (Q8) with the Juniper voice, text to speech.
    SpokenReplies,
}

impl VoicePart {
    /// Both parts, in the order the UI lists them.
    pub const ALL: [Self; 2] = [Self::SpeechInput, Self::SpokenReplies];

    /// The part's folder under [`MODELS_DIR`].
    pub const fn dir(self) -> &'static str {
        match self {
            Self::SpeechInput => "speech-input",
            Self::SpokenReplies => "spoken-replies",
        }
    }

    /// The part's files.
    pub fn files(self) -> impl Iterator<Item = &'static ModelFile> {
        MODEL_FILES.iter().filter(move |file| file.part == self)
    }
}

/// The models folder, under the [`crate::constants::DATA_DIR`] folder in `%LOCALAPPDATA%`.
pub const MODELS_DIR: &str = "models";

/// One model file, installed as `<MODELS_DIR>\<part dir>\<name>`.
#[derive(Debug)]
pub struct ModelFile {
    /// The part it belongs to.
    pub part: VoicePart,
    /// Its installed file name.
    pub name: &'static str,
    /// An immutable URL: a Hugging Face `resolve` URL at a pinned revision.
    pub url: &'static str,
    /// Its exact size.
    pub bytes: u64,
    /// Its SHA-256, lowercase hex.
    pub sha256: &'static str,
}

macro_rules! parakeet {
    ($file:literal) => {
        concat!("https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/resolve/2bda32ec70b097a55adaa07d9a7173915b43cc78/", $file)
    };
}
macro_rules! phonon {
    ($file:literal) => {
        concat!("https://huggingface.co/gradium/phonon-31m-en/resolve/92741c5b07d085ff840e26fea8ce43110b87852f/", $file)
    };
}

/// Every model file. Licenses and provenance are in `docs/development/voice.md`.
pub const MODEL_FILES: &[ModelFile] = &[
    ModelFile {
        part: VoicePart::SpeechInput,
        name: "encoder.int8.onnx",
        url: parakeet!("encoder.int8.onnx"),
        bytes: 652_184_281,
        sha256: "acfc2b4456377e15d04f0243af540b7fe7c992f8d898d751cf134c3a55fd2247",
    },
    ModelFile {
        part: VoicePart::SpeechInput,
        name: "decoder.int8.onnx",
        url: parakeet!("decoder.int8.onnx"),
        bytes: 11_845_275,
        sha256: "179e50c43d1a9de79c8a24149a2f9bac6eb5981823f2a2ed88d655b24248db4e",
    },
    ModelFile {
        part: VoicePart::SpeechInput,
        name: "joiner.int8.onnx",
        url: parakeet!("joiner.int8.onnx"),
        bytes: 6_355_277,
        sha256: "3164c13fc2821009440d20fcb5fdc78bff28b4db2f8d0f0b329101719c0948b3",
    },
    ModelFile {
        part: VoicePart::SpeechInput,
        name: "tokens.txt",
        url: parakeet!("tokens.txt"),
        bytes: 93_939,
        sha256: "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
    },
    ModelFile {
        part: VoicePart::SpokenReplies,
        name: "model.q8.gguf",
        url: phonon!("model.q8.gguf"),
        bytes: 64_512_352,
        sha256: "a7d9aad2cd6dc66473f24a08bc6d2939030ef4d751ef82349361fbef3272dbae",
    },
    ModelFile {
        part: VoicePart::SpokenReplies,
        name: "config.json",
        url: phonon!("config.json"),
        bytes: 4_634,
        sha256: "b5d0f88353638370d94035b7d73d7d6577c0554d78f8d7ecfd40490aae39ae85",
    },
    ModelFile {
        part: VoicePart::SpokenReplies,
        name: "tokenizer.json",
        url: phonon!("tokenizer.json"),
        bytes: 277_967,
        sha256: "691a00068fa7b989f2f3e5ae5daa2d1a5bb319f2d5360deafcfb19a8bb6e7936",
    },
    ModelFile {
        part: VoicePart::SpokenReplies,
        name: "juniper.safetensors",
        // Juniper's catalog id is its file name in gradium/phonon-voices.
        url: "https://huggingface.co/gradium/phonon-voices/resolve/0ee17205f254b5abc43b266c8a0f215565c352f7/voices/NIWIOv5vTi5eRMj8.safetensors",
        bytes: 256_136,
        sha256: "444378eb06b7c492b86e8fc7d13e4db1a3c0cf3e68891a960bf6f0a41a13ef35",
    },
];

/// One native library the brain loads at run time from beside `kumochid.exe`.
#[derive(Debug)]
pub struct NativeFile {
    /// Its file name.
    pub name: &'static str,
    /// Its exact size.
    pub bytes: u64,
    /// Its SHA-256, lowercase hex, checked before it's loaded.
    pub sha256: &'static str,
}

/// The CPU runtime's whole import closure beyond Windows and the VC runtime, from
/// [`NATIVE_ARCHIVE_URL`]'s `lib` folder. The audit is in `docs/development/voice-natives.md`.
pub const NATIVE_FILES: &[NativeFile] = &[
    NativeFile {
        name: "sherpa-onnx-c-api.dll",
        bytes: 3_925_504,
        sha256: "cf49e9ab5bac5e2d177fe14ae070cc4f52c791490333252150a745f9d03c1111",
    },
    NativeFile {
        name: "onnxruntime.dll",
        bytes: 17_136_128,
        sha256: "422d776ab0e3218260f7f628fcb84606aaa5c21116f720c8619a6da5e0b2e0f9",
    },
];

/// The release archive [`NATIVE_FILES`] come from, which nothing downloads at run time.
pub const NATIVE_ARCHIVE_URL: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/v2.0.1/sherpa-onnx-v2.0.1-win-x64-shared-MD-Release-lib.tar.bz2";

/// [`NATIVE_ARCHIVE_URL`]'s SHA-256, as GitHub's release page lists it.
pub const NATIVE_ARCHIVE_SHA256: &str =
    "c3c78220db8bebf97d5f932496165bb3897b163410427d7c021013439c80f183";

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{MODEL_FILES, NATIVE_ARCHIVE_SHA256, NATIVE_FILES, VoicePart};

    fn is_sha256(hex: &str) -> bool {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    fn is_plain_name(name: &str) -> bool {
        !name.is_empty()
            && !name.starts_with('.')
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
    }

    #[test]
    fn every_model_file_is_pinned_by_revision_size_and_hash() {
        let mut names = HashSet::new();
        for file in MODEL_FILES {
            assert!(is_sha256(file.sha256), "{} hash", file.name);
            assert!(file.bytes > 0, "{} size", file.name);
            assert!(
                is_plain_name(file.name),
                "{} is a plain file name",
                file.name
            );
            assert!(
                names.insert((file.part, file.name)),
                "{} is listed once",
                file.name
            );
            let rest = file
                .url
                .strip_prefix("https://huggingface.co/")
                .expect("models come from Hugging Face over HTTPS");
            let revision = rest
                .split("/resolve/")
                .nth(1)
                .and_then(|tail| tail.split('/').next())
                .expect("a resolve URL");
            assert!(
                revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit()),
                "{} is pinned to a commit, not a branch",
                file.name
            );
        }
        for part in VoicePart::ALL {
            assert!(part.files().count() > 0, "{part:?} has files");
        }
    }

    #[test]
    fn every_native_file_is_pinned_by_size_and_hash() {
        let mut names = HashSet::new();
        for file in NATIVE_FILES {
            assert!(is_sha256(file.sha256), "{} hash", file.name);
            assert!(file.bytes > 0, "{} size", file.name);
            assert!(
                is_plain_name(file.name),
                "{} is a plain file name",
                file.name
            );
            assert!(
                std::path::Path::new(file.name)
                    .extension()
                    .is_some_and(|ext| ext == "dll"),
                "{} is a DLL",
                file.name
            );
            assert!(names.insert(file.name), "{} is listed once", file.name);
        }
        assert!(is_sha256(NATIVE_ARCHIVE_SHA256), "archive hash");
    }
}
