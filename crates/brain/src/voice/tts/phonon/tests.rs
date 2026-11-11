use super::*;
use crate::test_support::Scratch;

#[test]
fn model_paths_refuse_redirected_phonon_models_before_starting_the_pool() {
    let scratch = Scratch::trusted("phonon-model-links");
    let actual = scratch.dir().join("actual");
    std::fs::create_dir_all(&actual).unwrap();
    let linked = scratch.dir().join("linked");
    std::fs::create_dir(&linked).unwrap();
    crate::platform::sherpa::tests::set_junction(&linked, &actual).unwrap();
    let before = POOL.get().copied();
    assert_eq!(Phonon::load(&linked).err(), Some(TtsError::ModelPath));
    for name in [CONFIG, WEIGHTS, TOKENIZER, VOICE] {
        std::fs::write(actual.join(name), b"fixture").unwrap();
    }
    let victim = scratch.dir().join("original");
    std::fs::write(&victim, b"fixture").unwrap();
    for name in [CONFIG, WEIGHTS, TOKENIZER, VOICE] {
        let file = actual.join(name);
        std::fs::remove_file(&file).unwrap();
        crate::platform::test_symlink_file(&victim, &file).expect("own model symlink");
        assert_eq!(Phonon::load(&actual).err(), Some(TtsError::ModelPath));
        assert_eq!(std::fs::read(&victim).unwrap(), b"fixture");
        std::fs::remove_file(file).unwrap();
        std::fs::write(actual.join(name), b"fixture").unwrap();
    }
    assert_eq!(POOL.get().copied(), before);
}
