use super::*;
use crate::test_support::Scratch;

#[test]
fn model_paths_startup_refuses_a_redirected_installed_folder_without_reading_it() {
    let scratch = Scratch::trusted("voice-startup-link");
    let actual = scratch.dir().join("actual");
    let root = scratch.dir().join("models");
    std::fs::create_dir_all(&actual).unwrap();
    std::fs::write(actual.join("sentinel"), b"unchanged").unwrap();
    std::fs::create_dir(&root).unwrap();
    crate::platform::sherpa::tests::set_junction(&root, &actual).unwrap();
    assert_eq!(
        whole(&root, VoicePart::SpeechInput),
        Err(ErrorKey::VoiceModelPath)
    );
    assert_eq!(
        std::fs::read(actual.join("sentinel")).unwrap(),
        b"unchanged"
    );
}

#[test]
fn model_paths_startup_refuses_a_link_in_each_constructor_file_set() {
    let scratch = Scratch::trusted("voice-startup-files");
    for part in VoicePart::ALL {
        let dir = scratch.dir().join(part.dir());
        std::fs::create_dir_all(&dir).unwrap();
        for file in part.files() {
            std::fs::write(dir.join(file.name), b"fixture").unwrap();
        }
        let victim = scratch.dir().join("original");
        std::fs::write(&victim, b"fixture").unwrap();
        for file in part.files() {
            let path = dir.join(file.name);
            std::fs::remove_file(&path).unwrap();
            crate::platform::test_symlink_file(&victim, &path).expect("own model symlink");
            assert_eq!(whole(scratch.dir(), part), Err(ErrorKey::VoiceModelPath));
            assert_eq!(std::fs::read(&victim).unwrap(), b"fixture");
            std::fs::remove_file(path).unwrap();
            std::fs::write(dir.join(file.name), b"fixture").unwrap();
        }
        assert_eq!(whole(scratch.dir(), part), Ok(false));
    }
}
