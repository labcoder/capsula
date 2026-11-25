//! The two socket pairs a parent-bound brain inherits: the shell keeps one end of each, and the
//! spawn places the other ends on the child's descriptors 3 (control) and 4 (data). These are
//! plain connected Unix streams; the session handshake on top of them is what authenticates.

use std::io;
use std::os::fd::{AsRawFd as _, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

/// The child's descriptor for the control leg.
pub(super) const CONTROL_FD: RawFd = 3;
/// The child's descriptor for the data leg.
pub(super) const DATA_FD: RawFd = 4;

/// The shell's ends. Dropping them closes both legs, so the brain sees EOF.
#[derive(Debug)]
pub(crate) struct ShellEnds {
    pub(crate) control: UnixStream,
    pub(crate) data: UnixStream,
}

/// The child's ends, held only until the spawn returns. Dropping them closes them.
#[derive(Debug)]
pub(crate) struct ChildEnds {
    pub(super) control: OwnedFd,
    pub(super) data: OwnedFd,
}

/// Makes both pairs, with `SO_NOSIGPIPE` on all four ends so a write to a closed peer fails
/// with EPIPE instead of a signal.
///
/// # Errors
/// Fails when a socket can't be made or configured; nothing is left open.
pub(crate) fn pairs() -> io::Result<(ShellEnds, ChildEnds)> {
    let (control, child_control) = UnixStream::pair()?;
    let (data, child_data) = UnixStream::pair()?;
    for end in [&control, &child_control, &data, &child_data] {
        super::child::no_sigpipe(end.as_raw_fd())?;
    }
    Ok((
        ShellEnds { control, data },
        ChildEnds {
            control: child_control.into(),
            data: child_data.into(),
        },
    ))
}

/// One spawn file action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Dup2 {
        from: RawFd,
        to: RawFd,
    },
    Close(RawFd),
    /// Opens `/dev/null` on the descriptor, for reading or for writing.
    Null {
        fd: RawFd,
        write: bool,
    },
}

/// The file actions that put `control` on 3 and `data` on 4 whatever numbers they have now,
/// even 3 and 4 swapped: both move above 4 first, then down, so neither move overwrites the
/// other. Standard input, output and error become `/dev/null`.
///
/// # Errors
/// `EMFILE` when the staging descriptors wouldn't fit under `limit`, the soft descriptor limit.
pub(super) fn plan(control: RawFd, data: RawFd, limit: u64) -> io::Result<[Action; 9]> {
    let high = control.max(data).max(DATA_FD) + 1;
    let (staged_control, staged_data) = (high, high + 1);
    if u64::try_from(staged_data).map_or(true, |fd| fd >= limit) {
        return Err(io::Error::from_raw_os_error(libc::EMFILE));
    }
    Ok([
        Action::Dup2 {
            from: control,
            to: staged_control,
        },
        Action::Dup2 {
            from: data,
            to: staged_data,
        },
        Action::Dup2 {
            from: staged_control,
            to: CONTROL_FD,
        },
        Action::Dup2 {
            from: staged_data,
            to: DATA_FD,
        },
        Action::Close(staged_control),
        Action::Close(staged_data),
        Action::Null {
            fd: 0,
            write: false,
        },
        Action::Null { fd: 1, write: true },
        Action::Null { fd: 2, write: true },
    ])
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{Read as _, Write as _};

    use super::*;

    /// Applies a plan to a descriptor table that maps numbers to what they refer to.
    fn apply(table: &mut BTreeMap<RawFd, &'static str>, plan: &[Action]) {
        for action in plan {
            match *action {
                Action::Dup2 { from, to } => {
                    let target = *table.get(&from).expect("dup2 from an open descriptor");
                    table.insert(to, target);
                }
                Action::Close(fd) => {
                    table.remove(&fd);
                }
                Action::Null { fd, write } => {
                    table.insert(fd, if write { "null-w" } else { "null-r" });
                }
            }
        }
    }

    #[test]
    fn the_legs_land_on_3_and_4_from_any_starting_slots() {
        for (control, data) in [
            (10, 11),
            (11, 10),
            (3, 4),
            (4, 3),
            (3, 9),
            (9, 4),
            (0, 1),
            (5, 3),
        ] {
            let mut table = BTreeMap::from([(control, "control"), (data, "data")]);
            let plan = plan(control, data, 1024).expect("plan");
            apply(&mut table, &plan);
            assert_eq!(
                table.get(&CONTROL_FD),
                Some(&"control"),
                "control from {control}, data from {data}"
            );
            assert_eq!(
                table.get(&DATA_FD),
                Some(&"data"),
                "control from {control}, data from {data}"
            );
            assert_eq!(
                (table.get(&0), table.get(&1), table.get(&2)),
                (Some(&"null-r"), Some(&"null-w"), Some(&"null-w")),
                "standard descriptors"
            );
            let staged = control.max(data).max(DATA_FD) + 1;
            assert!(
                !table.contains_key(&staged) && !table.contains_key(&(staged + 1)),
                "staging closed"
            );
        }
    }

    #[test]
    fn staging_at_or_over_the_descriptor_limit_is_refused() {
        assert!(plan(10, 11, 14).is_ok(), "13 fits under 14");
        for limit in [13, 12] {
            let refused = plan(10, 11, limit).unwrap_err();
            assert_eq!(refused.raw_os_error(), Some(libc::EMFILE), "limit {limit}");
        }
    }

    #[test]
    fn both_pairs_are_connected_and_ignore_sigpipe() {
        let (shell, child) = pairs().expect("pairs");
        for fd in [
            shell.control.as_raw_fd(),
            shell.data.as_raw_fd(),
            child.control.as_raw_fd(),
            child.data.as_raw_fd(),
        ] {
            assert!(
                super::super::child::has_no_sigpipe(fd),
                "SO_NOSIGPIPE on {fd}"
            );
        }
        let bound = Some(std::time::Duration::from_secs(5));
        let child_legs = [
            UnixStream::from(child.control),
            UnixStream::from(child.data),
        ];
        for (mut shell_leg, mut child_leg, byte) in [
            (&shell.control, &child_legs[0], b'c'),
            (&shell.data, &child_legs[1], b'd'),
        ] {
            shell_leg.set_write_timeout(bound).expect("timeout");
            child_leg.set_read_timeout(bound).expect("timeout");
            shell_leg.write_all(&[byte]).expect("write");
            let mut read = [0];
            child_leg
                .read_exact(&mut read)
                .expect("read within the bound");
            assert_eq!(read, [byte], "each shell end reaches its own child end");
        }
        drop(child_legs);
        // A spawn in another test can hold a copy of an end for an instant, so retry until the
        // close is seen.
        let by = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let error = loop {
            match (&shell.control).write_all(b"x") {
                Err(error) => break error,
                Ok(()) => assert!(std::time::Instant::now() < by, "the close was seen in time"),
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(
            error.kind(),
            io::ErrorKind::BrokenPipe,
            "EPIPE, not a signal"
        );
    }
}
