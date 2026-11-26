//! K-7 on the shell's side: the shell takes no core dumps, so none can hold the instance
//! capability it reads from the Keychain.

use std::io;

/// Sets `RLIMIT_CORE` to 0, soft and hard, then reads it back. Process-wide and idempotent: a limit
/// already at 0 is lowered to 0 again.
///
/// # Errors
/// The `setrlimit` or `getrlimit` error, or `PermissionDenied` when the readback isn't 0/0.
pub(crate) fn limit_core_dumps() -> io::Result<()> {
    limit_with(set_zero, core_limit)
}

fn limit_with(
    set: impl FnOnce() -> io::Result<()>,
    read: impl FnOnce() -> io::Result<(libc::rlim_t, libc::rlim_t)>,
) -> io::Result<()> {
    set()?;
    match read()? {
        (0, 0) => Ok(()),
        _ => Err(io::ErrorKind::PermissionDenied.into()),
    }
}

fn set_zero() -> io::Result<()> {
    let zero = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `zero` is a valid `rlimit` that outlives the call, which only reads it.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raw const zero) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `RLIMIT_CORE`'s soft and hard values.
pub(super) fn core_limit() -> io::Result<(libc::rlim_t, libc::rlim_t)> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is valid for one write of an `rlimit`.
    if unsafe { libc::getrlimit(libc::RLIMIT_CORE, &raw mut limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((limit.rlim_cur, limit.rlim_max))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn only_a_readback_of_0_0_is_hardened() {
        for (read, hardened) in [
            ((0, 0), true),
            ((0, 1), false),
            ((1, 0), false),
            ((libc::RLIM_INFINITY, libc::RLIM_INFINITY), false),
        ] {
            let result = limit_with(|| Ok(()), || Ok(read));
            assert_eq!(
                result.map_err(|error| error.kind()),
                if hardened {
                    Ok(())
                } else {
                    Err(io::ErrorKind::PermissionDenied)
                },
                "{read:?}"
            );
        }
    }

    #[test]
    fn a_failed_set_is_refused_before_any_readback() {
        let reads = Cell::new(0);
        let result = limit_with(
            || Err(io::ErrorKind::PermissionDenied.into()),
            || {
                reads.set(reads.get() + 1);
                Ok((0, 0))
            },
        );
        assert_eq!(
            result.map_err(|error| error.kind()),
            Err(io::ErrorKind::PermissionDenied)
        );
        assert_eq!(reads.get(), 0);
    }
}
