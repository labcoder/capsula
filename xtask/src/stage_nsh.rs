//! `stage-nsh`: a new folder holding a Gate `NativeShell` shell beside a Gate brain, under the
//! names the brain and the shell admit each other by. See `docs/development/shell-nsh.md`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail, ensure};
use kumochi_protocol::constants::{BRAIN_EXE, NSH_SHELL_EXE, ProductIdentity, SHELL_EXE};

use crate::hermetic::{self, BRAIN_ROLE, Profile, SHELL_ROLE};

/// Builds both images as one debug Gate build and stages them in the folder `args` names.
pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    hermetic::require_gate(ProductIdentity::ACTIVE)?;
    let folder = new_folder(args)?;
    let (images, _) = hermetic::build(
        &["kumochi-shell-nsh", "kumochi-brain"],
        Profile::Debug,
        None,
    )?;
    stage(&images, &folder)?;
    println!("{}", folder.display());
    Ok(())
}

/// The one argument, refused when it is missing, followed by another or already there.
fn new_folder(mut args: impl Iterator<Item = String>) -> Result<PathBuf> {
    let (Some(folder), None) = (args.next(), args.next()) else {
        bail!("stage-nsh takes one folder, which must not exist yet");
    };
    let folder = PathBuf::from(folder);
    ensure!(
        !folder.try_exists()?,
        "{} already exists; stage-nsh makes a new folder",
        folder.display()
    );
    Ok(folder)
}

/// Creates `folder` and copies the built shell and brain in `images` into it as [`SHELL_EXE`]
/// and [`BRAIN_EXE`], each copy checked for its role. A failure after the folder exists removes it.
fn stage(images: &Path, folder: &Path) -> Result<()> {
    fs::create_dir(folder).with_context(|| format!("creating {}", folder.display()))?;
    let staged = [
        (NSH_SHELL_EXE, SHELL_EXE, SHELL_ROLE),
        (BRAIN_EXE, BRAIN_EXE, BRAIN_ROLE),
    ]
    .into_iter()
    .try_for_each(|(built, name, role)| {
        let copy = folder.join(name);
        fs::copy(images.join(built), &copy).with_context(|| format!("staging {built}"))?;
        hermetic::require_compatible(&copy, role)
    });
    staged.or_else(|error| {
        fs::remove_dir_all(folder)
            .with_context(|| format!("{error:#}, and {} is left behind", folder.display()))?;
        Err(error)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hermetic::image;
    use crate::scratch;

    const REFUSED: &str = "kumochi.exe does not have the expected Gate identity, role and folder override; rebuild it";

    /// A build folder holding `shell` and `brain` under their built names, each when given.
    fn images(label: &str, shell: Option<&str>, brain: Option<&str>) -> scratch::Scratch {
        let dir = scratch::dir(label);
        for (name, bytes) in [(NSH_SHELL_EXE, shell), (BRAIN_EXE, brain)] {
            if let Some(bytes) = bytes {
                fs::write(dir.join(name), bytes).expect("image");
            }
        }
        dir
    }

    #[test]
    fn a_gate_pair_stages_under_the_names_the_brain_and_the_shell_admit() {
        let shell = image(SHELL_ROLE, ProductIdentity::GATE);
        let brain = image(BRAIN_ROLE, ProductIdentity::GATE);
        let built = images("stage-nsh-pair", Some(&shell), Some(&brain));
        let root = scratch::dir("stage-nsh-pair-out");
        let folder = root.join("staged");
        stage(&built, &folder).expect("a Gate pair stages");
        let mut names: Vec<_> = fs::read_dir(&folder)
            .expect("the staged folder")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        names.sort();
        assert_eq!(names, [SHELL_EXE, BRAIN_EXE]);
        assert_eq!(
            fs::read(folder.join(SHELL_EXE)).expect("shell"),
            shell.as_bytes()
        );
        assert_eq!(
            fs::read(folder.join(BRAIN_EXE)).expect("brain"),
            brain.as_bytes()
        );
    }

    #[test]
    fn an_existing_folder_is_refused_and_left_as_it_was() {
        let shell = image(SHELL_ROLE, ProductIdentity::GATE);
        let brain = image(BRAIN_ROLE, ProductIdentity::GATE);
        let built = images("stage-nsh-existing", Some(&shell), Some(&brain));
        let folder = scratch::dir("stage-nsh-existing-out");
        fs::write(folder.join("mine"), "the owner's file").expect("marker");
        let error = stage(&built, &folder).expect_err("an existing folder");
        assert_eq!(error.to_string(), format!("creating {}", folder.display()));
        let names: Vec<_> = fs::read_dir(&folder)
            .expect("the folder stays")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["mine"]);
        assert_eq!(
            fs::read(folder.join("mine")).expect("marker"),
            b"the owner's file"
        );
    }

    #[test]
    fn a_shell_that_isnt_a_gate_shell_is_refused_and_nothing_is_left() {
        let brain = image(BRAIN_ROLE, ProductIdentity::GATE);
        for (case, shell) in [
            ("dev", image(SHELL_ROLE, ProductIdentity::DEV)),
            ("release", image(SHELL_ROLE, ProductIdentity::RELEASE)),
            ("brain as shell", brain.clone()),
        ] {
            let built = images("stage-nsh-refused", Some(&shell), Some(&brain));
            let root = scratch::dir("stage-nsh-refused-out");
            let folder = root.join("staged");
            let error = stage(&built, &folder).expect_err(case);
            assert_eq!(format!("{error:#}"), REFUSED, "{case}");
            assert!(!folder.exists(), "{case} left the folder");
        }
    }

    #[test]
    fn a_missing_image_is_refused_and_nothing_is_left() {
        let shell = image(SHELL_ROLE, ProductIdentity::GATE);
        let brain = image(BRAIN_ROLE, ProductIdentity::GATE);
        for (missing, built) in [
            (
                NSH_SHELL_EXE,
                images("stage-nsh-no-shell", None, Some(&brain)),
            ),
            (BRAIN_EXE, images("stage-nsh-no-brain", Some(&shell), None)),
        ] {
            let root = scratch::dir("stage-nsh-missing-out");
            let folder = root.join("staged");
            let error = stage(&built, &folder).expect_err(missing);
            assert_eq!(error.to_string(), format!("staging {missing}"));
            assert!(!folder.exists(), "no {missing} left the folder");
        }
    }

    #[test]
    fn the_folder_is_one_argument_that_does_not_exist_yet() {
        let root = scratch::dir("stage-nsh-args");
        let new = root.join("new").display().to_string();
        let taken = root.display().to_string();
        assert_eq!(
            new_folder([new.clone()].into_iter()).expect("one new folder"),
            PathBuf::from(&new)
        );
        let refusal = "stage-nsh takes one folder, which must not exist yet";
        for args in [vec![], vec![new.clone(), new]] {
            let error = new_folder(args.into_iter()).expect_err("one argument");
            assert_eq!(error.to_string(), refusal);
        }
        let error = new_folder([taken.clone()].into_iter()).expect_err("an existing folder");
        assert_eq!(
            error.to_string(),
            format!("{taken} already exists; stage-nsh makes a new folder")
        );
    }
}
