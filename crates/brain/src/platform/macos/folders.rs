//! Per-user folder candidates, honoring the environment used by sandboxed runs.

use std::ffi::OsString;
use std::path::PathBuf;

use kumochi_protocol::constants;

pub(crate) const DATA_ROOT: &str = "HOME";

/// The per-user data candidate under `$HOME/Library/Application Support`, if HOME is absolute.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    data_dir_under(std::env::var_os(DATA_ROOT), constants::DATA_DIR)
}

/// Configuration and data share the same per-user Application Support candidate.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    data_dir()
}

fn data_dir_under(base: Option<OsString>, product: &str) -> Option<PathBuf> {
    let base = PathBuf::from(base?);
    base.is_absolute()
        .then(|| base.join("Library/Application Support").join(product))
}

#[cfg(test)]
mod tests {
    use super::data_dir_under;
    use kumochi_protocol::{constants, constants::ProductIdentity, message::ThreadId};
    use std::path::Path;

    #[test]
    fn the_data_folder_needs_an_absolute_home() {
        assert_eq!(data_dir_under(None, constants::DATA_DIR), None);
        for bad in ["", ".", "Users/Zoë Test"] {
            assert_eq!(
                data_dir_under(Some(bad.into()), constants::DATA_DIR),
                None,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn joins_the_exact_path_for_each_product_identity() {
        for (identity, expected) in [
            (
                ProductIdentity::RELEASE,
                "/fixture/Zoë Test/Library/Application Support/Kumochi",
            ),
            (
                ProductIdentity::DEV,
                "/fixture/Zoë Test/Library/Application Support/Kumochi Dev",
            ),
            (
                ProductIdentity::GATE,
                "/fixture/Zoë Test/Library/Application Support/Kumochi Gate",
            ),
        ] {
            assert_eq!(
                data_dir_under(Some("/fixture/Zoë Test".into()), identity.data_dir).as_deref(),
                Some(Path::new(expected))
            );
        }
    }

    #[test]
    fn config_reads_only_the_fixture_file_and_lookup_leaves_the_tree_unchanged() {
        let home = crate::scratch::dir("mac-folders");
        let expected = home
            .join("Library/Application Support")
            .join(constants::DATA_DIR);
        let file = expected.join("config.toml");
        let contents = "assistant_name = \"Nimbus\"\n";
        std::fs::create_dir_all(&expected).unwrap();
        std::fs::write(&file, contents).unwrap();
        let actual = data_dir_under(Some(home.as_os_str().to_owned()), constants::DATA_DIR);
        assert_eq!(actual.as_ref(), Some(&expected));
        let config = crate::config::Config::load(actual, ThreadId(1));
        assert_eq!(config.view().assistant_name.as_str(), "Nimbus");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), contents);
        for (parent, child) in [
            (home.path().to_owned(), "Library"),
            (home.join("Library"), "Application Support"),
            (
                home.join("Library/Application Support"),
                constants::DATA_DIR,
            ),
            (expected, "config.toml"),
        ] {
            let names: Vec<_> = parent
                .read_dir()
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(names, [child]);
        }
    }
}
