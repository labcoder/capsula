//! Read-only snapshots of the keyboard-layout registry keys, to prove a session changed nothing.

use anyhow::{Result, bail};
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, RegCloseKey, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW,
    RegQueryInfoKeyW,
};
use windows::core::{HSTRING, PWSTR};

/// The keys Windows reads for the user's keyboard layouts, under HKCU.
const ROOTS: [&str; 3] = [
    r"Keyboard Layout\Preload",
    r"Keyboard Layout\Substitutes",
    r"Control Panel\International\User Profile",
];

/// How reports name the hive the roots live in.
const HIVE: &str = "HKCU";

/// How deep a snapshot follows subkeys; deeper keys are recorded as truncated.
const DEPTH: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Value {
    name: String,
    kind: u32,
    data: Vec<u8>,
}

/// One key and everything under it, with the status codes of opening and reading it, so a key
/// that's missing or unreadable is compared too.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    name: String,
    opened: u32,
    read: u32,
    truncated: bool,
    values: Vec<Value>,
    keys: Vec<Key>,
}

/// The keyboard-layout keys at one moment. Taking it never fails and never writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Registry([Key; 3]);

impl Registry {
    pub(crate) fn snapshot() -> Self {
        Self(ROOTS.map(|path| read(HKEY_CURRENT_USER, path, 0)))
    }

    /// Succeeds only if both snapshots read every key completely and they're equal. A root that
    /// is missing both times is proven absent; any other open or read failure, or a truncated
    /// subtree, in either snapshot fails even when both match, since equal failures prove
    /// nothing. Names the first change and every unreadable root; repairs nothing.
    pub(crate) fn ensure_unchanged(&self, after: &Self) -> Result<()> {
        let mut problems = Vec::new();
        for (before, after) in self.0.iter().zip(&after.0) {
            if let Some(change) = first_change(before, after, HIVE) {
                problems.push(format!("changed at {change}"));
            }
        }
        for (when, snapshot) in [("before", self), ("after", after)] {
            for root in &snapshot.0 {
                if let Some(path) = unproven(root, HIVE) {
                    problems.push(format!("unreadable {when} at {path}"));
                }
            }
        }
        if problems.is_empty() {
            return Ok(());
        }
        bail!(
            "keyboard-layout registry not proven unchanged: {}; left as found",
            problems.join("; ")
        )
    }
}

/// The first key under `key` that wasn't read completely, unless it's a root (directly under
/// [`HIVE`]) that doesn't exist.
fn unproven(key: &Key, parent: &str) -> Option<String> {
    let path = format!(r"{parent}\{}", key.name);
    let absent = key.opened == ERROR_FILE_NOT_FOUND.0 || key.opened == ERROR_PATH_NOT_FOUND.0;
    if parent == HIVE && absent && !key.truncated {
        return None;
    }
    if key.truncated || key.opened != 0 || key.read != 0 {
        return Some(format!(
            "{path} (open {}, read {}, truncated {})",
            key.opened, key.read, key.truncated
        ));
    }
    key.keys.iter().find_map(|child| unproven(child, &path))
}

fn first_change(before: &Key, after: &Key, parent: &str) -> Option<String> {
    let path = format!(r"{parent}\{}", before.name);
    if before.name != after.name {
        return Some(format!(r"{parent}\{} (renamed)", after.name));
    }
    if (before.opened, before.read, before.truncated) != (after.opened, after.read, after.truncated)
    {
        return Some(format!(
            "{path} (open {}->{}, read {}->{})",
            before.opened, after.opened, before.read, after.read
        ));
    }
    let names = |values: &[Value]| {
        values
            .iter()
            .map(|value| value.name.clone())
            .collect::<Vec<_>>()
    };
    if names(&before.values) != names(&after.values) {
        return Some(format!("{path} (value set)"));
    }
    if let Some(value) = before
        .values
        .iter()
        .zip(&after.values)
        .find(|(b, a)| b != a)
    {
        return Some(format!("{path} (value {})", value.0.name));
    }
    if before.keys.len() != after.keys.len() {
        return Some(format!("{path} (subkey set)"));
    }
    before
        .keys
        .iter()
        .zip(&after.keys)
        .find_map(|(before, after)| first_change(before, after, &path))
}

fn read(parent: HKEY, name: &str, depth: usize) -> Key {
    let mut key = Key {
        name: name.to_owned(),
        opened: 0,
        read: 0,
        truncated: depth >= DEPTH,
        values: Vec::new(),
        keys: Vec::new(),
    };
    if key.truncated {
        return key;
    }
    let mut handle = HKEY::default();
    // SAFETY: opens for KEY_READ only; handle is a writable output closed below.
    let opened = unsafe {
        RegOpenKeyExW(
            parent,
            &HSTRING::from(name),
            None,
            KEY_READ,
            &raw mut handle,
        )
    };
    key.opened = opened.0;
    if opened != ERROR_SUCCESS {
        return key;
    }
    key.read = contents(handle, &mut key, depth).0;
    // SAFETY: closes only the handle opened above.
    let closed = unsafe { RegCloseKey(handle) };
    if key.read == 0 {
        key.read = closed.0;
    }
    key.values.sort_by(|a, b| a.name.cmp(&b.name));
    key.keys.sort_by(|a, b| a.name.cmp(&b.name));
    key
}

/// Reads `handle`'s values and subkeys into `key`; returns the first failure, or success.
fn contents(handle: HKEY, key: &mut Key, depth: usize) -> WIN32_ERROR {
    let (mut max_key, mut max_name, mut max_data) = (0, 0, 0);
    // SAFETY: the handle is open for reading and every requested size output is writable.
    let info = unsafe {
        RegQueryInfoKeyW(
            handle,
            None,
            None,
            None,
            None,
            Some(&raw mut max_key),
            None,
            None,
            Some(&raw mut max_name),
            Some(&raw mut max_data),
            None,
            None,
        )
    };
    if info != ERROR_SUCCESS {
        return info;
    }
    let mut name = vec![0u16; usize::try_from(max_name.max(max_key)).unwrap_or(0) + 1];
    let mut data = vec![0u8; usize::try_from(max_data).unwrap_or(0)];
    for index in 0.. {
        let mut name_length = u32::try_from(name.len()).unwrap_or(u32::MAX);
        let mut data_length = u32::try_from(data.len()).unwrap_or(u32::MAX);
        let mut kind = 0;
        // SAFETY: both buffers are writable for the lengths passed with them.
        let status = unsafe {
            RegEnumValueW(
                handle,
                index,
                Some(PWSTR(name.as_mut_ptr())),
                &raw mut name_length,
                None,
                Some(&raw mut kind),
                Some(data.as_mut_ptr()),
                Some(&raw mut data_length),
            )
        };
        if status == ERROR_NO_MORE_ITEMS {
            break;
        }
        if status != ERROR_SUCCESS {
            return status;
        }
        key.values.push(Value {
            name: String::from_utf16_lossy(&name[..usize::try_from(name_length).unwrap_or(0)]),
            kind,
            data: data[..usize::try_from(data_length).unwrap_or(0)].to_vec(),
        });
    }
    let mut children = Vec::new();
    for index in 0.. {
        let mut name_length = u32::try_from(name.len()).unwrap_or(u32::MAX);
        // SAFETY: name is writable for the length passed with it; no class or time is read.
        let status = unsafe {
            RegEnumKeyExW(
                handle,
                index,
                Some(PWSTR(name.as_mut_ptr())),
                &raw mut name_length,
                None,
                None,
                None,
                None,
            )
        };
        if status == ERROR_NO_MORE_ITEMS {
            break;
        }
        if status != ERROR_SUCCESS {
            return status;
        }
        children.push(String::from_utf16_lossy(
            &name[..usize::try_from(name_length).unwrap_or(0)],
        ));
    }
    key.keys = children
        .iter()
        .map(|child| read(handle, child, depth + 1))
        .collect();
    ERROR_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str, values: &[(&str, &[u8])], keys: Vec<Key>) -> Key {
        Key {
            name: name.to_owned(),
            opened: 0,
            read: 0,
            truncated: false,
            values: values
                .iter()
                .map(|(name, data)| Value {
                    name: (*name).to_owned(),
                    kind: 1,
                    data: data.to_vec(),
                })
                .collect(),
            keys,
        }
    }

    fn profile(language: &[u8]) -> Key {
        key(
            "User Profile",
            &[("Languages", b"en-US")],
            vec![key("en-US", &[("0409:00000409", language)], Vec::new())],
        )
    }

    fn registry(preload: Key) -> Registry {
        Registry([preload, key("Substitutes", &[], Vec::new()), profile(b"1")])
    }

    fn preload() -> Key {
        key("Preload", &[("1", b"00000409")], Vec::new())
    }

    #[test]
    fn an_equal_recursive_read_is_proven_unchanged() {
        assert!(
            registry(preload())
                .ensure_unchanged(&registry(preload()))
                .is_ok()
        );
    }

    #[test]
    fn a_root_missing_both_times_is_proven_absent() {
        let missing = || Key {
            opened: 2,
            ..key("Preload", &[], Vec::new())
        };
        assert!(
            registry(missing())
                .ensure_unchanged(&registry(missing()))
                .is_ok()
        );
    }

    #[test]
    fn the_same_access_failure_twice_proves_nothing() {
        let denied = || Key {
            opened: 5,
            ..key("Preload", &[], Vec::new())
        };
        let error = registry(denied())
            .ensure_unchanged(&registry(denied()))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(r"unreadable before at HKCU\Preload (open 5, read 0, truncated false)"),
            "{error}"
        );
        assert!(
            error.contains(r"unreadable after at HKCU\Preload"),
            "{error}"
        );
    }

    #[test]
    fn the_same_truncated_subtree_twice_proves_nothing() {
        let deep = || {
            key(
                "Preload",
                &[],
                vec![Key {
                    truncated: true,
                    ..key("deep", &[], Vec::new())
                }],
            )
        };
        let error = registry(deep())
            .ensure_unchanged(&registry(deep()))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(
                r"unreadable before at HKCU\Preload\deep (open 0, read 0, truncated true)"
            ),
            "{error}"
        );
    }

    #[test]
    fn a_real_change_is_still_named() {
        let changed = key("Preload", &[("1", b"00000407")], Vec::new());
        let error = registry(preload())
            .ensure_unchanged(&registry(changed))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(r"changed at HKCU\Preload (value 1)"),
            "{error}"
        );
    }

    #[test]
    fn an_identical_tree_has_no_change() {
        assert_eq!(first_change(&profile(b"1"), &profile(b"1"), "HKCU"), None);
    }

    #[test]
    fn a_nested_value_change_names_its_key_and_value() {
        assert_eq!(
            first_change(&profile(b"1"), &profile(b"2"), "HKCU").as_deref(),
            Some(r"HKCU\User Profile\en-US (value 0409:00000409)")
        );
    }

    #[test]
    fn a_key_appearing_or_failing_to_open_counts_as_a_change() {
        let missing = Key {
            opened: 2,
            ..key("Substitutes", &[], Vec::new())
        };
        let created = key("Substitutes", &[], Vec::new());
        assert_eq!(
            first_change(&missing, &created, "HKCU").as_deref(),
            Some(r"HKCU\Substitutes (open 2->0, read 0->0)")
        );
        let added = key(
            "Preload",
            &[("1", b"00000409"), ("2", b"00000407")],
            Vec::new(),
        );
        let before = key("Preload", &[("1", b"00000409")], Vec::new());
        assert_eq!(
            first_change(&before, &added, "HKCU").as_deref(),
            Some(r"HKCU\Preload (value set)")
        );
    }
}
