//! The microphone's privacy permission, read without prompting. macOS feeds silence to input it
//! hasn't authorized, so capture checks this before it creates any audio object.

use objc2::runtime::AnyClass;
use objc2_avf_audio::{AVAudioApplication, AVAudioApplicationRecordPermission};

use crate::platform::microphone::MicError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Permission {
    Granted,
    Undetermined,
    Denied,
}

/// The current permission. Before macOS 14 `AVAudioApplication` doesn't exist, so it reads as
/// denied rather than guessed.
pub(crate) fn permission() -> Permission {
    if AnyClass::get(c"AVAudioApplication").is_none() {
        return Permission::Denied;
    }
    // SAFETY: the class exists (macOS 14 and later); this getter takes nothing and returns the
    // retained shared instance.
    let application = unsafe { AVAudioApplication::sharedInstance() };
    // SAFETY: a read-only property getter on that live instance; it never prompts.
    from_raw(unsafe { application.recordPermission() })
}

/// Any value but the two documented non-denials reads as denied.
fn from_raw(raw: AVAudioApplicationRecordPermission) -> Permission {
    match raw {
        AVAudioApplicationRecordPermission::Granted => Permission::Granted,
        AVAudioApplicationRecordPermission::Undetermined => Permission::Undetermined,
        _ => Permission::Denied,
    }
}

/// Runs `open` only when granted. Undetermined refuses too: the prompt belongs to an explicit
/// user action, never to a hold.
pub(crate) fn admit<T>(
    permission: Permission,
    open: impl FnOnce() -> Result<T, MicError>,
) -> Result<T, MicError> {
    match permission {
        Permission::Granted => open(),
        Permission::Undetermined | Permission::Denied => Err(MicError::Permission),
    }
}

#[cfg(test)]
mod tests {
    use objc2_avf_audio::AVAudioApplicationRecordPermission as Raw;

    use super::{MicError, Permission, admit, from_raw};

    #[test]
    fn only_the_documented_granted_code_reads_as_granted() {
        assert_eq!(from_raw(Raw(0x6772_6e74)), Permission::Granted);
        assert_eq!(from_raw(Raw(0x756e_6474)), Permission::Undetermined);
        assert_eq!(from_raw(Raw(0x6465_6e79)), Permission::Denied);
        assert_eq!(from_raw(Raw(0)), Permission::Denied);
        assert_eq!(from_raw(Raw(0x7273_7472)), Permission::Denied);
    }

    #[test]
    fn only_a_granted_permission_reaches_the_opener() {
        for refused in [Permission::Undetermined, Permission::Denied] {
            let mut opened = 0;
            let result = admit(refused, || {
                opened += 1;
                Ok(7)
            });
            assert_eq!((result, opened), (Err(MicError::Permission), 0));
        }
        let mut opened = 0;
        let result = admit(Permission::Granted, || {
            opened += 1;
            Ok(7)
        });
        assert_eq!((result, opened), (Ok(7), 1));
        assert_eq!(
            admit::<()>(Permission::Granted, || Err(MicError::Unavailable)),
            Err(MicError::Unavailable)
        );
    }
}
