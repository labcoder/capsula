//! The per-user application data roots.
//!
//! An ordinary build keeps the route each row takes today: the environment's `LOCALAPPDATA` or
//! `APPDATA`, or the Known Folder. Gate-identity and debug builds take one root pair from one
//! source, both override variables or both Known Folders, and refuse a half-set or unusable
//! override and an environment an old driver redirected, so a run never lands in the real profile
//! by accident.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf, Prefix};

use kumochi_protocol::constants::{
    HONOURS_APPDATA_OVERRIDE, LOCAL_APPDATA_OVERRIDE, ROAMING_APPDATA_OVERRIDE,
};
use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, FOLDERID_RoamingAppData};

use super::admit::admitted;
use super::known_folder;

/// Why a per-user folder can't be resolved. It carries no path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AppDataError {
    /// The folder this row reads isn't an absolute path, or Windows reports none.
    #[error("the application data folder is unavailable")]
    Unavailable,
    /// The override is half set, or a half isn't a usable folder below the local
    /// application data folder.
    #[error("the application data override is invalid")]
    InvalidOverride,
    /// No override is set, but `LOCALAPPDATA` or `APPDATA` points away from its Known Folder.
    #[error("the environment redirects application data without the override")]
    OldDriver,
}

/// The route an ordinary build takes today for a row. Gate and debug builds ignore it. Cut 8
/// deletes it, when ordinary builds move to the Known Folders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Today {
    /// The environment's `LOCALAPPDATA` (or `APPDATA` for the roaming root).
    Environment,
    /// The Known Folder.
    KnownFolder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Build {
    /// Reads only the route a row takes today, never the override.
    Ordinary,
    /// Honours the override pair, else both Known Folders.
    Honours,
}

const BUILD: Build = if HONOURS_APPDATA_OVERRIDE {
    Build::Honours
} else {
    Build::Ordinary
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Half {
    Local,
    Roaming,
}

/// What [`choose`] reads, each looked up only when the build and row need it.
struct Inputs<'a> {
    overrides: &'a dyn Fn(Half) -> Option<OsString>,
    environment: &'a dyn Fn(Half) -> Option<OsString>,
    known: &'a dyn Fn(Half) -> Option<PathBuf>,
    canonical: &'a dyn Fn(&Path) -> Option<PathBuf>,
}

/// Where a root comes from.
#[derive(Debug, PartialEq, Eq)]
enum Root {
    /// One folder: the environment's or the Known Folder.
    Plain(PathBuf),
    /// The validated override pair, as given.
    Override(Pair),
}

/// The override's local and roaming roots, as the variables spell them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pair {
    pub(crate) local: PathBuf,
    pub(crate) roaming: PathBuf,
}

/// The local application data root for a row whose ordinary route is `today`. Under an
/// override, the first call in a process admits the pair before it returns either root.
///
/// # Errors
/// `Unavailable` when the row's folder isn't known, and in Gate and debug builds
/// `InvalidOverride` or `OldDriver` as [`AppDataError`] describes.
pub fn local_app_data(today: Today) -> Result<PathBuf, AppDataError> {
    resolve(today, Half::Local)
}

/// The roaming application data root. An ordinary build reads the environment's `APPDATA`, as
/// today.
///
/// # Errors
/// As [`local_app_data`].
pub fn roaming_app_data() -> Result<PathBuf, AppDataError> {
    resolve(Today::Environment, Half::Roaming)
}

fn resolve(today: Today, half: Half) -> Result<PathBuf, AppDataError> {
    let inputs = Inputs {
        overrides: &|half| {
            if HONOURS_APPDATA_OVERRIDE {
                std::env::var_os(match half {
                    Half::Local => LOCAL_APPDATA_OVERRIDE,
                    Half::Roaming => ROAMING_APPDATA_OVERRIDE,
                })
            } else {
                None
            }
        },
        environment: &|half| {
            std::env::var_os(match half {
                Half::Local => "LOCALAPPDATA",
                Half::Roaming => "APPDATA",
            })
        },
        known: &|half| {
            known_folder(match half {
                Half::Local => FOLDERID_LocalAppData,
                Half::Roaming => FOLDERID_RoamingAppData,
            })
            .ok()
        },
        canonical: &|path| std::fs::canonicalize(path).ok(),
    };
    match choose(BUILD, today, half, &inputs)? {
        Root::Plain(path) => Ok(path),
        Root::Override(pair) => admitted(&pair).map(|()| match half {
            Half::Local => pair.local,
            Half::Roaming => pair.roaming,
        }),
    }
}

/// The one decision. Every call in a Gate or debug build checks the whole pair, so the local and
/// roaming roots can't disagree about whether the override holds.
fn choose(
    build: Build,
    today: Today,
    half: Half,
    inputs: &Inputs<'_>,
) -> Result<Root, AppDataError> {
    if build == Build::Ordinary {
        let path = match today {
            Today::Environment => (inputs.environment)(half).map(PathBuf::from),
            Today::KnownFolder => (inputs.known)(half),
        };
        return path
            .filter(|path| path.is_absolute())
            .map(Root::Plain)
            .ok_or(AppDataError::Unavailable);
    }
    match (
        (inputs.overrides)(Half::Local),
        (inputs.overrides)(Half::Roaming),
    ) {
        (Some(local), Some(roaming)) => Ok(Root::Override(validated(
            Pair {
                local: local.into(),
                roaming: roaming.into(),
            },
            inputs,
        )?)),
        (None, None) => {
            for half in [Half::Local, Half::Roaming] {
                if let (Some(set), Some(known)) = ((inputs.environment)(half), (inputs.known)(half))
                    && !same_path(Path::new(&set), &known)
                {
                    return Err(AppDataError::OldDriver);
                }
            }
            (inputs.known)(half)
                .filter(|path| path.is_absolute())
                .map(Root::Plain)
                .ok_or(AppDataError::Unavailable)
        }
        _ => Err(AppDataError::InvalidOverride),
    }
}

/// The cheap early refusal: each half is an absolute path on a disk that exists, strictly below
/// the canonical local application data folder once resolved, and the halves don't overlap. Startup still holds
/// the chain of each half as given, so a link anywhere on it refuses there.
fn validated(pair: Pair, inputs: &Inputs<'_>) -> Result<Pair, AppDataError> {
    let anchor = (inputs.known)(Half::Local)
        .and_then(|known| (inputs.canonical)(&known))
        .and_then(|path| on_disk(&path))
        .ok_or(AppDataError::InvalidOverride)?;
    let below = |path: &Path| {
        let resolved = on_disk(path)
            .filter(|path| path.is_absolute())
            .and_then(|path| (inputs.canonical)(&path))
            .and_then(|path| on_disk(&path))?;
        (resolved.starts_with(&anchor) && resolved != anchor).then_some(resolved)
    };
    let (Some(local), Some(roaming)) = (below(&pair.local), below(&pair.roaming)) else {
        return Err(AppDataError::InvalidOverride);
    };
    if local.starts_with(&roaming) || roaming.starts_with(&local) {
        return Err(AppDataError::InvalidOverride);
    }
    Ok(pair)
}

/// `path` in plain drive form (`C:\…`), or `None` unless it's on a drive letter: no UNC,
/// device or volume path.
fn on_disk(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return None;
    };
    let (Prefix::Disk(drive) | Prefix::VerbatimDisk(drive)) = prefix.kind() else {
        return None;
    };
    if components.next() != Some(Component::RootDir) {
        return None;
    }
    let mut plain = PathBuf::from(format!("{}:\\", char::from(drive)));
    plain.extend(components);
    Some(plain)
}

/// Whether two spellings name the same folder, ignoring case and a trailing separator.
fn same_path(set: &Path, known: &Path) -> bool {
    let tidy = |path: &Path| path.to_string_lossy().trim_end_matches('\\').to_lowercase();
    tidy(set) == tidy(known)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use kumochi_protocol::constants::ProductIdentity;

    use super::*;

    /// Values for each input, local then roaming, with a fixed canonical map: a path missing from
    /// it doesn't exist, and one mapped elsewhere resolves there.
    struct Fake {
        overrides: [Option<&'static str>; 2],
        environment: [Option<&'static str>; 2],
        known: [Option<&'static str>; 2],
        canonical: &'static [(&'static str, &'static str)],
    }

    const ANCHOR: &str = r"C:\Users\me\AppData\Local";
    const ROAMING: &str = r"C:\Users\me\AppData\Roaming";
    const LOCAL_RUN: &str = r"C:\Users\me\AppData\Local\gate-runs\run-1\local";
    const ROAMING_RUN: &str = r"C:\Users\me\AppData\Local\gate-runs\run-1\roaming";
    const EXISTING: &[(&str, &str)] = &[
        (ANCHOR, r"\\?\C:\Users\me\AppData\Local"),
        (
            LOCAL_RUN,
            r"\\?\C:\Users\me\AppData\Local\gate-runs\run-1\local",
        ),
        (
            ROAMING_RUN,
            r"\\?\C:\Users\me\AppData\Local\gate-runs\run-1\roaming",
        ),
    ];

    impl Fake {
        /// The override pair under the anchor, with a redirected environment beside it.
        fn conflicting() -> Self {
            Self {
                overrides: [Some(LOCAL_RUN), Some(ROAMING_RUN)],
                environment: [Some(r"D:\old-driver\local"), Some(r"D:\old-driver\roaming")],
                known: [Some(ANCHOR), Some(ROAMING)],
                canonical: EXISTING,
            }
        }

        fn choose(&self, build: Build, today: Today, half: Half) -> Result<Root, AppDataError> {
            let at = |half| usize::from(half == Half::Roaming);
            let canonical: HashMap<PathBuf, PathBuf> = self
                .canonical
                .iter()
                .map(|&(from, to)| (PathBuf::from(from), PathBuf::from(to)))
                .collect();
            choose(
                build,
                today,
                half,
                &Inputs {
                    overrides: &|half| self.overrides[at(half)].map(OsString::from),
                    environment: &|half| self.environment[at(half)].map(OsString::from),
                    known: &|half| self.known[at(half)].map(PathBuf::from),
                    canonical: &|path| canonical.get(path).cloned(),
                },
            )
        }
    }

    /// An override pair, local then roaming, with the canonical map it's checked against.
    type Case = ([&'static str; 2], &'static [(&'static str, &'static str)]);

    fn plain(path: &str) -> Root {
        Root::Plain(PathBuf::from(path))
    }

    fn pair() -> Root {
        Root::Override(Pair {
            local: LOCAL_RUN.into(),
            roaming: ROAMING_RUN.into(),
        })
    }

    #[test]
    fn an_ordinary_build_takes_todays_route_and_never_reads_the_override() {
        let fake = Fake::conflicting();
        let never = |_: Half| -> Option<OsString> { panic!("an ordinary build read the override") };
        let at = |half| usize::from(half == Half::Roaming);
        let inputs = Inputs {
            overrides: &never,
            environment: &|half| fake.environment[at(half)].map(OsString::from),
            known: &|half| fake.known[at(half)].map(PathBuf::from),
            canonical: &|_| panic!("an ordinary build checked an override"),
        };
        let ordinary = |today, half| choose(Build::Ordinary, today, half, &inputs);
        assert_eq!(
            ordinary(Today::Environment, Half::Local),
            Ok(plain(r"D:\old-driver\local"))
        );
        assert_eq!(ordinary(Today::KnownFolder, Half::Local), Ok(plain(ANCHOR)));
        assert_eq!(
            ordinary(Today::Environment, Half::Roaming),
            Ok(plain(r"D:\old-driver\roaming"))
        );
    }

    #[test]
    fn an_ordinary_build_needs_an_absolute_folder_and_a_missing_roaming_one_is_only_unavailable() {
        for bad in [
            None,
            Some(""),
            Some("."),
            Some("AppData"),
            Some(r"\Users\me"),
            Some("C:AppData"),
        ] {
            let fake = Fake {
                environment: [Some(r"C:\Users\me\AppData\Local"), bad],
                ..Fake::conflicting()
            };
            assert_eq!(
                fake.choose(Build::Ordinary, Today::Environment, Half::Roaming),
                Err(AppDataError::Unavailable),
                "{bad:?}"
            );
            assert_eq!(
                fake.choose(Build::Ordinary, Today::Environment, Half::Local),
                Ok(plain(r"C:\Users\me\AppData\Local")),
                "{bad:?}"
            );
        }
        let fake = Fake {
            known: [None, None],
            ..Fake::conflicting()
        };
        assert_eq!(
            fake.choose(Build::Ordinary, Today::KnownFolder, Half::Local),
            Err(AppDataError::Unavailable)
        );
    }

    #[test]
    fn a_full_override_wins_for_every_row_and_ignores_a_redirected_environment() {
        let fake = Fake::conflicting();
        for today in [Today::Environment, Today::KnownFolder] {
            for half in [Half::Local, Half::Roaming] {
                assert_eq!(
                    fake.choose(Build::Honours, today, half),
                    Ok(pair()),
                    "{today:?} {half:?}"
                );
            }
        }
    }

    #[test]
    fn a_half_set_override_is_refused_for_both_halves() {
        for overrides in [[Some(LOCAL_RUN), None], [None, Some(ROAMING_RUN)]] {
            let fake = Fake {
                overrides,
                environment: [Some(ANCHOR), Some(ROAMING)],
                ..Fake::conflicting()
            };
            for half in [Half::Local, Half::Roaming] {
                assert_eq!(
                    fake.choose(Build::Honours, Today::Environment, half),
                    Err(AppDataError::InvalidOverride),
                    "{overrides:?} {half:?}"
                );
            }
        }
    }

    #[test]
    fn an_override_off_the_anchor_or_overlapping_is_refused() {
        let cases: [Case; 10] = [
            ([r"gate\local", ROAMING_RUN], EXISTING),
            ([r"C:gate\local", ROAMING_RUN], EXISTING),
            ([r"\\server\share\local", ROAMING_RUN], EXISTING),
            (
                [r"\\.\C:\Users\me\AppData\Local\run", ROAMING_RUN],
                EXISTING,
            ),
            (
                [r"C:\Users\me\AppData\Local\missing", ROAMING_RUN],
                EXISTING,
            ),
            ([ANCHOR, ROAMING_RUN], EXISTING),
            (
                [r"C:\Users\me", ROAMING_RUN],
                &[
                    (ANCHOR, r"\\?\C:\Users\me\AppData\Local"),
                    (r"C:\Users\me", r"\\?\C:\Users\me"),
                    (
                        ROAMING_RUN,
                        r"\\?\C:\Users\me\AppData\Local\gate-runs\run-1\roaming",
                    ),
                ],
            ),
            (
                [LOCAL_RUN, ROAMING_RUN],
                &[
                    (ANCHOR, r"\\?\C:\Users\me\AppData\Local"),
                    (LOCAL_RUN, r"\\?\C:\Elsewhere\local"),
                    (
                        ROAMING_RUN,
                        r"\\?\C:\Users\me\AppData\Local\gate-runs\run-1\roaming",
                    ),
                ],
            ),
            (
                [LOCAL_RUN, ROAMING_RUN],
                &[
                    (ANCHOR, r"\\?\C:\Users\me\AppData\Local"),
                    (LOCAL_RUN, r"\\?\UNC\server\share\local"),
                    (
                        ROAMING_RUN,
                        r"\\?\C:\Users\me\AppData\Local\gate-runs\run-1\roaming",
                    ),
                ],
            ),
            (
                [
                    LOCAL_RUN,
                    r"C:\Users\me\AppData\Local\gate-runs\run-1\local\roaming",
                ],
                &[
                    (ANCHOR, r"\\?\C:\Users\me\AppData\Local"),
                    (
                        LOCAL_RUN,
                        r"\\?\C:\Users\me\AppData\Local\gate-runs\run-1\local",
                    ),
                    (
                        r"C:\Users\me\AppData\Local\gate-runs\run-1\local\roaming",
                        r"\\?\C:\Users\me\AppData\Local\gate-runs\run-1\local\roaming",
                    ),
                ],
            ),
        ];
        for ([local, roaming], canonical) in cases {
            for overrides in [[Some(local), Some(roaming)], [Some(roaming), Some(local)]] {
                let fake = Fake {
                    overrides,
                    canonical,
                    ..Fake::conflicting()
                };
                assert_eq!(
                    fake.choose(Build::Honours, Today::Environment, Half::Local),
                    Err(AppDataError::InvalidOverride),
                    "{overrides:?}"
                );
            }
        }
        let same = Fake {
            overrides: [Some(LOCAL_RUN), Some(LOCAL_RUN)],
            ..Fake::conflicting()
        };
        assert_eq!(
            same.choose(Build::Honours, Today::Environment, Half::Roaming),
            Err(AppDataError::InvalidOverride)
        );
    }

    #[test]
    fn without_an_override_only_a_redirected_environment_is_an_old_driver() {
        let matching = Fake {
            overrides: [None, None],
            environment: [Some(r"c:\users\ME\AppData\Local\"), None],
            ..Fake::conflicting()
        };
        for today in [Today::Environment, Today::KnownFolder] {
            assert_eq!(
                matching.choose(Build::Honours, today, Half::Local),
                Ok(plain(ANCHOR))
            );
            assert_eq!(
                matching.choose(Build::Honours, today, Half::Roaming),
                Ok(plain(ROAMING))
            );
        }
        for environment in [
            [Some(r"D:\old-driver\local"), Some(ROAMING)],
            [None, Some(r"D:\old-driver\roaming")],
        ] {
            let redirected = Fake {
                overrides: [None, None],
                environment,
                ..Fake::conflicting()
            };
            for half in [Half::Local, Half::Roaming] {
                assert_eq!(
                    redirected.choose(Build::Honours, Today::Environment, half),
                    Err(AppDataError::OldDriver),
                    "{environment:?} {half:?}"
                );
            }
        }
        let unknown = Fake {
            overrides: [None, None],
            environment: [Some(ANCHOR), None],
            known: [None, Some(ROAMING)],
            ..Fake::conflicting()
        };
        assert_eq!(
            unknown.choose(Build::Honours, Today::Environment, Half::Local),
            Err(AppDataError::Unavailable)
        );
    }

    #[test]
    fn the_build_honours_the_override_unless_it_carries_the_release_names() {
        let expected = if ProductIdentity::ACTIVE == ProductIdentity::RELEASE {
            Build::Ordinary
        } else {
            Build::Honours
        };
        assert_eq!(BUILD, expected, "{:?}", ProductIdentity::ACTIVE);
    }
}
