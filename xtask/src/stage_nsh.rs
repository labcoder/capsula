//! `stage-nsh`: a new folder holding a Gate `NativeShell` shell beside a Gate brain, under the
//! names the brain and the shell admit each other by. See `docs/development/shell-nsh.md`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail, ensure};
use kumochi_protocol::constants::{BRAIN_EXE, NSH_SHELL_EXE, ProductIdentity, SHELL_EXE};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

use crate::hermetic::{self, BRAIN_ROLE, Profile, SHELL_ROLE};

/// Each image's name as built, the name it is staged under, and its role.
const PAIR: [(&str, &str, &str); 2] = [
    (NSH_SHELL_EXE, SHELL_EXE, SHELL_ROLE),
    (BRAIN_EXE, BRAIN_EXE, BRAIN_ROLE),
];

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

/// The one argument, refused before the long build when it is missing, followed by another,
/// already there or without a folder to be made in.
fn new_folder(mut args: impl Iterator<Item = String>) -> Result<PathBuf> {
    let (Some(folder), None) = (args.next(), args.next()) else {
        bail!("stage-nsh takes one folder, which must not exist yet");
    };
    let folder = PathBuf::from(folder);
    // Not `try_exists`: a dangling link is an entry too.
    match fs::symlink_metadata(&folder) {
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("checking {}", folder.display()));
        }
        Ok(_) => bail!(
            "{} already exists; stage-nsh makes a new folder",
            folder.display()
        ),
    }
    let parent = folder
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    ensure!(
        parent.unwrap_or(Path::new(".")).is_dir(),
        "{} has no folder to be made in",
        folder.display()
    );
    Ok(folder)
}

/// Creates `folder`, which must not be there yet, and copies the built shell and brain in
/// `images` into it under their staged names. Each image is checked for its role before the
/// folder is made, so a pair that doesn't fit leaves nothing behind.
fn stage(images: &Path, folder: &Path) -> Result<()> {
    for (built, _, role) in PAIR {
        hermetic::require_compatible(&images.join(built), role)?;
    }
    fs::create_dir(folder).with_context(|| format!("creating {}", folder.display()))?;
    fill(images, folder)
}

/// Copies the pair into `folder` and checks each copy.
///
/// shortcut: a failed copy removes nothing, so it can't remove a file that isn't this task's, and
/// the error says the folder is left incomplete; roll back through owned handles if a caller
/// ever needs a failure to leave nothing.
fn fill(images: &Path, folder: &Path) -> Result<()> {
    copy_pair(images, folder).with_context(|| {
        format!(
            "{} is left behind incomplete; nothing in it was removed",
            folder.display()
        )
    })
}

/// The copies, with `folder` held so it can't be renamed or removed meanwhile, and never over a
/// file that is already there. The parent is taken to be a folder nothing else is changing: one
/// swapped in before the hold would be filled instead.
fn copy_pair(images: &Path, folder: &Path) -> Result<()> {
    let _held = hold(folder).with_context(|| format!("holding {}", folder.display()))?;
    for (built, name, role) in PAIR {
        let copy = folder.join(name);
        let staging = || format!("staging {built}");
        let mut from = File::open(images.join(built)).with_context(staging)?;
        let mut to = File::create_new(&copy).with_context(staging)?;
        io::copy(&mut from, &mut to).with_context(staging)?;
        drop(to);
        hermetic::require_compatible(&copy, role)?;
    }
    Ok(())
}

/// Opens `folder` without sharing delete, so nothing renames or removes it while this is held.
fn hold(folder: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(folder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hermetic::image;
    use crate::scratch;

    const REFUSED: &str = "kumochi-nsh.exe does not have the expected Gate identity, role and folder override; rebuild it";

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

    /// The names in `folder`, sorted.
    fn names(folder: &Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = fs::read_dir(folder)
            .expect("the folder")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_gate_pair_stages_under_the_names_the_brain_and_the_shell_admit() {
        let shell = image(SHELL_ROLE, ProductIdentity::GATE);
        let brain = image(BRAIN_ROLE, ProductIdentity::GATE);
        let built = images("stage-nsh-pair", Some(&shell), Some(&brain));
        let root = scratch::dir("stage-nsh-pair-out");
        let folder = root.join("staged");
        stage(&built, &folder).expect("a Gate pair stages");
        assert_eq!(names(&folder), [SHELL_EXE, BRAIN_EXE]);
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
        assert_eq!(names(&folder), ["mine"]);
        assert_eq!(
            fs::read(folder.join("mine")).expect("marker"),
            b"the owner's file"
        );
    }

    #[test]
    fn a_shell_that_isnt_a_gate_shell_is_refused_before_the_folder_is_made() {
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
            assert!(!folder.exists(), "{case} made the folder");
        }
    }

    #[test]
    fn a_missing_image_is_refused_before_the_folder_is_made() {
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
            assert_eq!(error.to_string(), format!("{missing} is unavailable"));
            assert!(!folder.exists(), "no {missing} made the folder");
        }
    }

    #[test]
    fn a_failed_copy_writes_over_nothing_and_removes_nothing() {
        let shell = image(SHELL_ROLE, ProductIdentity::GATE);
        let brain = image(BRAIN_ROLE, ProductIdentity::GATE);
        let built = images("stage-nsh-taken", Some(&shell), Some(&brain));
        let folder = scratch::dir("stage-nsh-taken-out");
        fs::write(folder.join(BRAIN_EXE), "someone else's brain").expect("theirs");
        let error = fill(&built, &folder).expect_err("the brain's name is taken");
        assert_eq!(
            error.to_string(),
            format!(
                "{} is left behind incomplete; nothing in it was removed",
                folder.display()
            )
        );
        assert_eq!(
            error.chain().nth(1).expect("the cause").to_string(),
            format!("staging {BRAIN_EXE}")
        );
        assert_eq!(names(&folder), [SHELL_EXE, BRAIN_EXE]);
        assert_eq!(
            fs::read(folder.join(SHELL_EXE)).expect("the copy made first"),
            shell.as_bytes()
        );
        assert_eq!(
            fs::read(folder.join(BRAIN_EXE)).expect("theirs"),
            b"someone else's brain"
        );
    }

    #[test]
    fn a_held_folder_cant_be_renamed_until_the_hold_drops() {
        let root = scratch::dir("stage-nsh-hold");
        let folder = root.join("staged");
        let moved = root.join("moved");
        fs::create_dir(&folder).expect("the staged folder");
        let held = hold(&folder).expect("hold");
        let error = fs::rename(&folder, &moved).expect_err("the folder is held");
        // ERROR_SHARING_VIOLATION.
        assert_eq!(error.raw_os_error(), Some(32));
        assert!(folder.is_dir() && !moved.exists());
        drop(held);
        fs::rename(&folder, &moved).expect("the hold is gone");
        assert!(moved.is_dir() && !folder.exists());
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
        let orphan = root.join("missing").join("new").display().to_string();
        let error = new_folder([orphan.clone()].into_iter()).expect_err("no parent folder");
        assert_eq!(
            error.to_string(),
            format!("{orphan} has no folder to be made in")
        );
    }
}
