use std::ffi::OsString;
use std::path::PathBuf;

/// Returns the Application Support candidate under an absolute `HOME`.
///
/// This checks no filesystem state. The storage owner must admit the path before use.
#[must_use]
pub fn support_dir() -> Option<PathBuf> {
    support_dir_under(std::env::var_os("HOME"))
}

fn support_dir_under(home: Option<OsString>) -> Option<PathBuf> {
    let home = PathBuf::from(home?);
    home.is_absolute()
        .then(|| home.join("Library/Application Support"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::support_dir_under;

    #[test]
    fn application_support_needs_an_absolute_home() {
        assert_eq!(support_dir_under(None), None);
        for home in ["", ".", "Users/Zoë Test"] {
            assert_eq!(support_dir_under(Some(home.into())), None, "{home:?}");
        }
    }

    #[test]
    fn application_support_joins_the_home_without_a_product_folder() {
        assert_eq!(
            support_dir_under(Some("/fixture/Zoë Test".into())).as_deref(),
            Some(Path::new("/fixture/Zoë Test/Library/Application Support"))
        );
    }
}
