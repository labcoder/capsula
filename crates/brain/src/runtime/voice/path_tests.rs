use super::*;
use crate::test_support::Scratch;

#[test]
fn model_review_h3_refused_removal_preserves_all_original_names_and_bytes() {
    let scratch = Scratch::trusted("model-tree-refusal");
    let models = scratch.dir().join("models");
    let part = models.join(VoicePart::SpeechInput.dir());
    std::fs::create_dir_all(part.join("b-folder")).unwrap();
    std::fs::write(part.join("a-model"), b"ordinary").unwrap();
    std::fs::write(part.join("b-folder").join("model"), b"nested").unwrap();
    let outside = scratch.dir().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("sentinel"), b"outside").unwrap();
    let link = part.join("z-link");
    std::fs::create_dir(&link).unwrap();
    crate::platform::sherpa::tests::set_junction(&link, &outside).unwrap();
    let target = std::fs::read_link(&link).unwrap();
    assert_eq!(delete(&models, VoicePart::SpeechInput), Folder::Present);
    let mut names = std::fs::read_dir(&part)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["a-model", "b-folder", "z-link"]);
    assert_eq!(std::fs::read(part.join("a-model")).unwrap(), b"ordinary");
    assert_eq!(
        std::fs::read(part.join("b-folder").join("model")).unwrap(),
        b"nested"
    );
    assert_eq!(std::fs::read_link(link).unwrap(), target);
    assert_eq!(std::fs::read(outside.join("sentinel")).unwrap(), b"outside");
}

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
