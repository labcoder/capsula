//! Data protection keychain generic passwords, accessed only by exact target name.
//!
//! Dormant: only tests construct it, until a signed, provisioned brain can be admitted to it.

use std::ptr::{self, NonNull};

use kumochi_protocol::Secret;
use kumochi_protocol::constants::CREDENTIAL_SERVICE;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_core_foundation::{
    CFBoolean, CFData, CFDictionary, CFIndex, CFRetained, CFString, CFType, kCFAllocatorNull,
};
use objc2_security::{
    SecItemAdd, SecItemCopyMatching, SecItemDelete, SecItemUpdate, errSecAllocate,
    errSecDuplicateItem, errSecItemNotFound, errSecSuccess, kSecAttrAccessible,
    kSecAttrAccessibleWhenUnlockedThisDeviceOnly, kSecAttrAccount, kSecAttrService,
    kSecAttrSynchronizable, kSecClass, kSecClassGenericPassword, kSecMatchLimit, kSecMatchLimitOne,
    kSecReturnData, kSecUseAuthenticationContext, kSecUseDataProtectionKeychain, kSecValueData,
};
use zeroize::Zeroizing;

use crate::platform::credential::{CredentialStore, CredentialStoreError, SecretBytes};

/// Windows' generic credential blob limit, kept so both stores accept the same blobs.
pub(crate) const MAX_BLOB: usize = 2_560;

/// Windows' generic target limit in UTF-16 units, kept so both stores accept the same names.
const MAX_TARGET: usize = 32_767;

#[link(name = "LocalAuthentication", kind = "framework")]
unsafe extern "C" {}

/// The data protection keychain, never the file-based login keychain, with UI refused.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MacCredentialStore;

impl CredentialStore for MacCredentialStore {
    fn get(&self, target: &str) -> Result<Option<SecretBytes>, CredentialStoreError> {
        let item = Item::new(target)?;
        let keys = Keys::get();
        let query = item.query(&[
            (keys.return_data, CFBoolean::new(true)),
            (keys.match_limit, keys.match_limit_one),
        ]);
        let mut result: *const CFType = ptr::null();
        // SAFETY: the query is a live CFString-keyed dictionary; result is a writable out pointer.
        let status = unsafe { SecItemCopyMatching(query.as_opaque(), &raw mut result) };
        if status == errSecItemNotFound {
            return Ok(None);
        }
        if status != errSecSuccess {
            return Err(CredentialStoreError::Read(status));
        }
        let result = NonNull::new(result.cast_mut()).ok_or(CredentialStoreError::InvalidBlob)?;
        // SAFETY: a successful copy returns one owned reference, released once by this owner.
        let result = unsafe { CFRetained::from_raw(result) };
        let data = result
            .downcast::<CFData>()
            .map_err(|_| CredentialStoreError::InvalidBlob)?;
        if usize::try_from(data.length()).map_or(true, |len| len > MAX_BLOB) {
            return Err(CredentialStoreError::InvalidBlob);
        }
        // shortcut: the OS-owned CFData can't be wiped; only our copy is zeroizing.
        Ok(Some(Secret::new(Zeroizing::new(data.to_vec()))))
    }

    fn set(&self, target: &str, secret: &SecretBytes) -> Result<(), CredentialStoreError> {
        validate(target)?;
        let bytes = secret.expose();
        if bytes.len() > MAX_BLOB {
            return Err(CredentialStoreError::BlobTooLarge);
        }
        let item = Item::new(target)?;
        let keys = Keys::get();
        let len = CFIndex::try_from(bytes.len()).expect("the bounded blob length fits CFIndex");
        // SAFETY: reads an immutable CoreFoundation constant.
        let no_free = unsafe { kCFAllocatorNull };
        // SAFETY: the view borrows the caller's live buffer only until it drops at the end of this
        // call, and kCFAllocatorNull never frees those bytes.
        let data = unsafe { CFData::with_bytes_no_copy(None, bytes.as_ptr(), len, no_free) }
            .ok_or(CredentialStoreError::Write(errSecAllocate))?;
        let add = item.query(&[
            (keys.accessible, keys.when_unlocked_this_device_only),
            (keys.value_data, &data),
        ]);
        // SAFETY: the attributes are a live CFString-keyed dictionary; no result is requested.
        let status = unsafe { SecItemAdd(add.as_opaque(), ptr::null_mut()) };
        if status != errSecDuplicateItem {
            return if status == errSecSuccess {
                Ok(())
            } else {
                Err(CredentialStoreError::Write(status))
            };
        }
        let query = item.query(&[]);
        let update = CFDictionary::<CFString, CFType>::from_slices(&[keys.value_data], &[&data]);
        // SAFETY: the query and update are live CFString-keyed dictionaries for this call.
        let status = unsafe { SecItemUpdate(query.as_opaque(), update.as_opaque()) };
        if status == errSecSuccess {
            Ok(())
        } else {
            Err(CredentialStoreError::Write(status))
        }
    }

    fn delete(&self, target: &str) -> Result<(), CredentialStoreError> {
        let item = Item::new(target)?;
        let query = item.query(&[]);
        // SAFETY: the query is a live CFString-keyed dictionary that names one exact item.
        let status = unsafe { SecItemDelete(query.as_opaque()) };
        if status == errSecSuccess || status == errSecItemNotFound {
            Ok(())
        } else {
            Err(CredentialStoreError::Delete(status))
        }
    }
}

fn validate(target: &str) -> Result<(), CredentialStoreError> {
    if target.is_empty() || target.contains('\0') || target.encode_utf16().count() > MAX_TARGET {
        return Err(CredentialStoreError::InvalidTarget);
    }
    Ok(())
}

/// One exact item's unique key, with the fresh no-UI context every call carries.
struct Item {
    service: CFRetained<CFString>,
    account: CFRetained<CFString>,
    context: Retained<AnyObject>,
}

impl Item {
    fn new(target: &str) -> Result<Self, CredentialStoreError> {
        validate(target)?;
        Ok(Self {
            service: CFString::from_str(CREDENTIAL_SERVICE),
            account: CFString::from_str(target),
            context: no_ui_context()?,
        })
    }

    fn query(&self, extra: &[(&CFString, &CFType)]) -> CFRetained<CFDictionary<CFString, CFType>> {
        let keys = Keys::get();
        let context: *const AnyObject = &raw const *self.context;
        // SAFETY: every Objective-C object is a valid CFTypeRef, and the context outlives the
        // returned dictionary's use within the caller's operation.
        let context = unsafe { &*context.cast::<CFType>() };
        let mut pairs = vec![
            (keys.class, keys.generic_password.as_ref()),
            (keys.service, self.service.as_ref()),
            (keys.account, self.account.as_ref()),
            (keys.data_protection, CFBoolean::new(true).as_ref()),
            (keys.synchronizable, CFBoolean::new(false).as_ref()),
            (keys.context, context),
        ];
        pairs.extend_from_slice(extra);
        let (keys, values): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
        CFDictionary::from_slices(&keys, &values)
    }
}

/// A fresh `LAContext` that refuses any authentication UI; refused setup refuses the operation.
fn no_ui_context() -> Result<Retained<AnyObject>, CredentialStoreError> {
    let class = AnyClass::get(c"LAContext").ok_or(CredentialStoreError::Unsupported)?;
    // SAFETY: +new on LAContext takes no arguments and returns an owned object or nil.
    let context: Option<Retained<AnyObject>> = unsafe { msg_send![class, new] };
    let context = context.ok_or(CredentialStoreError::Unsupported)?;
    // SAFETY: interactionNotAllowed is a BOOL property of LAContext since macOS 10.13.
    let () = unsafe { msg_send![&context, setInteractionNotAllowed: true] };
    // SAFETY: reads the same BOOL property back.
    let refused: bool = unsafe { msg_send![&context, interactionNotAllowed] };
    refused
        .then_some(context)
        .ok_or(CredentialStoreError::Unsupported)
}

/// The Security framework's query keys and values.
struct Keys {
    class: &'static CFString,
    generic_password: &'static CFString,
    service: &'static CFString,
    account: &'static CFString,
    data_protection: &'static CFString,
    synchronizable: &'static CFString,
    context: &'static CFString,
    accessible: &'static CFString,
    when_unlocked_this_device_only: &'static CFString,
    value_data: &'static CFString,
    return_data: &'static CFString,
    match_limit: &'static CFString,
    match_limit_one: &'static CFString,
}

impl Keys {
    fn get() -> Self {
        // SAFETY: reads immutable Security framework constants that the loader initialized.
        unsafe {
            Self {
                class: kSecClass,
                generic_password: kSecClassGenericPassword,
                service: kSecAttrService,
                account: kSecAttrAccount,
                data_protection: kSecUseDataProtectionKeychain,
                synchronizable: kSecAttrSynchronizable,
                context: kSecUseAuthenticationContext,
                accessible: kSecAttrAccessible,
                when_unlocked_this_device_only: kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
                value_data: kSecValueData,
                return_data: kSecReturnData,
                match_limit: kSecMatchLimit,
                match_limit_one: kSecMatchLimitOne,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::platform::credential::conformance;
    use crate::platform::credential::fixture::Fixture;

    use super::*;

    #[test]
    fn invalid_input_is_refused_before_the_keychain() {
        conformance::refuses_invalid_input(&MacCredentialStore, MAX_BLOB, "test-only-target");
    }

    #[test]
    fn a_no_ui_context_is_set_up_for_every_call() {
        let first = no_ui_context().expect("LocalAuthentication is linked");
        let second = no_ui_context().expect("a second context");
        assert!(
            !ptr::eq(&raw const *first, &raw const *second),
            "each call gets its own context"
        );
    }

    #[test]
    #[ignore = "needs a test host signed and provisioned for the data protection keychain"]
    fn a_signed_host_meets_the_shared_contract() {
        let nonce = getrandom::u64().expect("random test name");
        let target = format!("{CREDENTIAL_SERVICE}-credential-test-{nonce:016x}");
        let _fixture = Fixture::with_store(MacCredentialStore, vec![target.clone()]);
        conformance::round_trip(&MacCredentialStore, &target, MAX_BLOB);
    }
}
