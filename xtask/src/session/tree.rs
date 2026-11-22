//! The processes one launch of the product runs, by role.

use anyhow::{Context as _, Result, ensure};

use crate::platform::Entry;

/// Each role a launch runs, with the exit code a graceful quit gives it: the brain ends its
/// workers' job, so they exit 1. The workers follow the brain's supervisors (speech input's at
/// voice start, then the screen's), so a worker added to the brain is one more line here.
pub(crate) const ROLES: [(&str, u32); 4] = [
    ("shell", 0),
    ("brain", 0),
    ("screen-worker", 1),
    ("speech-worker", 1),
];

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Startup {
    /// Every role's pid, in [`ROLES`]' order.
    Complete([(&'static str, u32); ROLES.len()]),
    /// The first role with no process yet.
    Missing(&'static str),
}

/// Sorts `entries` into the tree `shell_pid` started. Fails on a product process outside it, a
/// worker of an unknown or repeated role, or a worker whose role `identify` can't read.
pub(crate) fn product_tree(
    shell_pid: u32,
    entries: &[Entry],
    identify: impl Fn(u32) -> Result<&'static str>,
) -> Result<Startup> {
    ensure!(
        entries
            .iter()
            .any(|entry| entry.pid == shell_pid && entry.shell),
        "the launched shell is missing"
    );
    let mut tree = ROLES.map(|(role, _)| (role, None));
    tree[0].1 = Some(shell_pid);
    if let Some(brain) = entries
        .iter()
        .find(|entry| !entry.shell && entry.parent == shell_pid)
    {
        tree[1].1 = Some(brain.pid);
        for entry in entries
            .iter()
            .filter(|entry| !entry.shell && entry.parent == brain.pid)
        {
            let role = identify(entry.pid)?;
            let slot = tree[2..]
                .iter_mut()
                .find(|(known, _)| *known == role)
                .context("unexpected product helper role")?;
            ensure!(slot.1.is_none(), "duplicate product helper role");
            slot.1 = Some(entry.pid);
        }
    }
    let found = tree.iter().filter_map(|(_, pid)| *pid).collect::<Vec<_>>();
    ensure!(
        unchanged(entries, &found),
        "a product process outside the launched tree is running; refusing a contaminated run"
    );
    if let Some((role, _)) = tree.iter().find(|(_, pid)| pid.is_none()) {
        return Ok(Startup::Missing(role));
    }
    Ok(Startup::Complete(
        tree.map(|(role, pid)| (role, pid.unwrap_or_default())),
    ))
}

/// Whether `entries` are exactly the processes `expected` names.
pub(crate) fn unchanged(entries: &[Entry], expected: &[u32]) -> bool {
    entries.len() == expected.len()
        && expected
            .iter()
            .all(|pid| entries.iter().filter(|entry| entry.pid == *pid).count() == 1)
}

#[cfg(test)]
mod tests {
    use super::{Startup, product_tree, unchanged};
    use crate::platform::Entry;

    fn helper(pid: u32, parent: u32) -> Entry {
        Entry {
            pid,
            parent,
            shell: false,
        }
    }

    fn launch() -> Vec<Entry> {
        vec![
            helper(40, 20),
            helper(20, 10),
            Entry {
                pid: 10,
                parent: 1,
                shell: true,
            },
            helper(30, 20),
        ]
    }

    fn identify(pid: u32) -> anyhow::Result<&'static str> {
        match pid {
            30 => Ok("screen-worker"),
            40 => Ok("speech-worker"),
            _ => anyhow::bail!("unknown fixture helper"),
        }
    }

    const COMPLETE: Startup = Startup::Complete([
        ("shell", 10),
        ("brain", 20),
        ("screen-worker", 30),
        ("speech-worker", 40),
    ]);

    #[test]
    fn four_processes_form_the_tree_in_any_enumeration_order() {
        let mut entries = launch();
        assert_eq!(product_tree(10, &entries, identify).unwrap(), COMPLETE);
        entries.reverse();
        assert_eq!(product_tree(10, &entries, identify).unwrap(), COMPLETE);
        assert!(unchanged(&entries, &[10, 20, 30, 40]));
        assert!(!unchanged(&entries, &[10, 20, 30, 41]));
    }

    #[test]
    fn a_launch_still_starting_names_the_first_missing_process() {
        let entries = launch();
        let without = |pid| {
            entries
                .iter()
                .filter(|entry| entry.pid != pid)
                .map(|entry| Entry {
                    pid: entry.pid,
                    parent: entry.parent,
                    shell: entry.shell,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            product_tree(10, &without(30), identify).unwrap(),
            Startup::Missing("screen-worker")
        );
        assert_eq!(
            product_tree(10, &without(40), identify).unwrap(),
            Startup::Missing("speech-worker")
        );
        assert_eq!(
            product_tree(10, &entries[2..3], identify).unwrap(),
            Startup::Missing("brain")
        );
        assert_eq!(
            product_tree(10, &entries[1..3], identify).unwrap(),
            Startup::Missing("screen-worker"),
            "when both workers are missing"
        );
    }

    #[test]
    fn a_stranger_or_a_wrong_worker_is_refused() {
        let mut entries = launch();
        entries.push(helper(50, 1));
        product_tree(10, &entries, identify).expect_err("outside process isn't startup");
        entries.pop();
        assert_eq!(
            product_tree(10, &entries, |_| Ok("screen-worker"))
                .unwrap_err()
                .to_string(),
            "duplicate product helper role"
        );
        assert_eq!(
            product_tree(10, &entries, |_| Ok("unknown"))
                .unwrap_err()
                .to_string(),
            "unexpected product helper role"
        );
        entries[0].parent = 1;
        product_tree(10, &entries, identify).expect_err("orphaned worker isn't startup");
        assert_eq!(
            product_tree(11, &launch(), identify)
                .unwrap_err()
                .to_string(),
            "the launched shell is missing"
        );
    }
}
