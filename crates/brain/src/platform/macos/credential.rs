//! Data protection keychain generic passwords, accessed only by exact target name.
//!
//! The native store, reached only through `platform::credential::admitted_store`; only a
//! `first-light` Dev brain opens it, after checking every name earlier builds wrote is absent.

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use kumochi_protocol::Secret;
use kumochi_protocol::constants::CREDENTIAL_SERVICE;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_core_foundation::{
    CFAllocator, CFAllocatorContext, CFBoolean, CFData, CFDictionary, CFIndex, CFOptionFlags,
    CFRetained, CFString, CFType,
};
use objc2_security::{
    SecItemAdd, SecItemCopyMatching, SecItemDelete, SecItemUpdate, SecTask, errSecAllocate,
    errSecDuplicateItem, errSecItemNotFound, errSecSuccess, kSecAttrAccessGroup,
    kSecAttrAccessible, kSecAttrAccessibleWhenUnlockedThisDeviceOnly, kSecAttrAccount,
    kSecAttrService, kSecAttrSynchronizable, kSecClass, kSecClassGenericPassword, kSecMatchLimit,
    kSecMatchLimitOne, kSecReturnData, kSecUseAuthenticationContext, kSecUseDataProtectionKeychain,
    kSecValueData,
};
use zeroize::Zeroizing;

use crate::platform::credential::{CredentialStore, CredentialStoreError, SecretBytes};

/// Windows' generic credential blob limit, kept so both stores accept the same blobs.
const MAX_BLOB: usize = 2_560;

/// Windows' generic target limit in UTF-16 units, kept so both stores accept the same names.
const MAX_TARGET: usize = 32_767;

/// The provisioned App ID, which is also the process's private keychain access group.
const APPLICATION_IDENTIFIER: &str = "com.apple.application-identifier";

#[link(name = "LocalAuthentication", kind = "framework")]
unsafe extern "C" {}

type Query = CFRetained<CFDictionary<CFString, CFType>>;

/// The data protection keychain, never the file-based login keychain, pinned to one access
/// group, with UI refused.
#[derive(Debug, Clone)]
pub(crate) struct MacCredentialStore {
    group: String,
}

impl MacCredentialStore {
    /// Pins the application identifier this process claims as its only access group, and refuses
    /// a process that claims none before any Keychain call. Whether the OS honors the claim
    /// (validated provisioning) is proved only by a signed run, never by this check.
    pub(crate) fn admitted() -> Result<Self, CredentialStoreError> {
        let store = Self::grouped()?;
        #[cfg(feature = "first-light")]
        legacy_absent(&LEGACY, &mut || true, |target| store.status(target))?;
        Ok(store)
    }

    /// First light's legacy check on the admitted group, before custody: the check `admitted`
    /// would make, asking `live` before each name, and latched for the vault that opens later.
    #[cfg(feature = "first-light")]
    pub(crate) fn preflight(live: &mut dyn FnMut() -> bool) -> Result<(), CredentialStoreError> {
        let store = Self::grouped()?;
        legacy_absent(&LEGACY, live, |target| store.status(target))
    }

    /// The store pinned to the application identifier this process claims.
    fn grouped() -> Result<Self, CredentialStoreError> {
        // SAFETY: creates an owned task object for this process with the default allocator.
        let task = unsafe { SecTask::from_self(None) }.ok_or(CredentialStoreError::Unsupported)?;
        let name = CFString::from_static_str(APPLICATION_IDENTIFIER);
        // SAFETY: a null error pointer is allowed; the result is an owned value or none.
        let value = unsafe { task.value_for_entitlement(&name, ptr::null_mut()) }
            .ok_or(CredentialStoreError::Unsupported)?;
        let group = value
            .downcast::<CFString>()
            .map_err(|_| CredentialStoreError::Unsupported)?
            .to_string();
        if group.is_empty() {
            return Err(CredentialStoreError::Unsupported);
        }
        Ok(Self { group })
    }

    /// The exact item's status alone: no data or attributes are asked for, and no result is
    /// taken, so nothing is copied out.
    #[cfg(feature = "first-light")]
    fn status(&self, target: &str) -> Result<i32, CredentialStoreError> {
        let query = self.status_query(target)?;
        // SAFETY: the query is a live CFString-keyed dictionary; a null result asks for nothing.
        Ok(unsafe { SecItemCopyMatching(query.as_opaque(), ptr::null_mut()) })
    }

    /// One exact item's unique key in the pinned group, with a fresh no-UI context.
    fn query(
        &self,
        target: &str,
        extra: &[(&CFString, &CFType)],
    ) -> Result<Query, CredentialStoreError> {
        let keys = Keys::get();
        let context = no_ui_context()?;
        let context: *const AnyObject = &raw const *context;
        // SAFETY: this LAContext is an NSObject, which CoreFoundation retains and releases as a
        // CFType; the dictionary built below retains it before the owner above drops.
        let context = unsafe { &*context.cast::<CFType>() };
        let group = CFString::from_str(&self.group);
        let service = CFString::from_str(CREDENTIAL_SERVICE);
        let account = CFString::from_str(target);
        let mut pairs = vec![
            (keys.class, keys.generic_password.as_ref()),
            (keys.group, group.as_ref()),
            (keys.service, service.as_ref()),
            (keys.account, account.as_ref()),
            (keys.data_protection, CFBoolean::new(true).as_ref()),
            (keys.synchronizable, CFBoolean::new(false).as_ref()),
            (keys.context, context),
        ];
        pairs.extend_from_slice(extra);
        let (keys, values): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
        Ok(CFDictionary::from_slices(&keys, &values))
    }

    /// One exact item's key and a match limit of one, with nothing to return.
    #[cfg(feature = "first-light")]
    fn status_query(&self, target: &str) -> Result<Query, CredentialStoreError> {
        validate(target)?;
        let keys = Keys::get();
        self.query(target, &[(keys.match_limit, keys.match_limit_one)])
    }

    fn read_query(&self, target: &str) -> Result<Query, CredentialStoreError> {
        let keys = Keys::get();
        self.query(
            target,
            &[
                (keys.return_data, CFBoolean::new(true)),
                (keys.match_limit, keys.match_limit_one),
            ],
        )
    }

    fn add_attributes(&self, target: &str, data: &CFData) -> Result<Query, CredentialStoreError> {
        let keys = Keys::get();
        self.query(
            target,
            &[
                (keys.accessible, keys.when_unlocked_this_device_only),
                (keys.value_data, data),
            ],
        )
    }

    /// Adds, or on a duplicate replaces only the data, each call with its own context.
    fn write(&self, target: &str, data: &CFData) -> Result<(), CredentialStoreError> {
        let add = self.add_attributes(target, data)?;
        // SAFETY: the attributes are a live CFString-keyed dictionary; no result is requested.
        let status = unsafe { SecItemAdd(add.as_opaque(), ptr::null_mut()) };
        if status != errSecDuplicateItem {
            return succeeded(status, CredentialStoreError::Write);
        }
        let query = self.query(target, &[])?;
        let update = update_attributes(data);
        // SAFETY: the query and update are live CFString-keyed dictionaries for this call.
        let status = unsafe { SecItemUpdate(query.as_opaque(), update.as_opaque()) };
        succeeded(status, CredentialStoreError::Write)
    }
}

impl CredentialStore for MacCredentialStore {
    fn get(&self, target: &str) -> Result<Option<SecretBytes>, CredentialStoreError> {
        validate(target)?;
        let query = self.read_query(target)?;
        let mut result: *const CFType = ptr::null();
        // SAFETY: the query is a live CFString-keyed dictionary; result is a writable out pointer.
        let status = unsafe { SecItemCopyMatching(query.as_opaque(), &raw mut result) };
        if !found(status)? {
            return Ok(None);
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
        let data = owned_view(bytes)?;
        self.write(target, &data)
    }

    fn delete(&self, target: &str) -> Result<(), CredentialStoreError> {
        // SAFETY: the query is a live CFString-keyed dictionary that names one exact item.
        self.delete_by(target, |query| unsafe { SecItemDelete(query.as_opaque()) })
    }
}

impl MacCredentialStore {
    /// `delete` with its one native call made by `native`, so a test can see it isn't made.
    fn delete_by(
        &self,
        target: &str,
        native: impl FnOnce(&Query) -> i32,
    ) -> Result<(), CredentialStoreError> {
        validate(target)?;
        #[cfg(feature = "first-light")]
        if let Some(answer) = legacy_delete(&LEGACY, target) {
            return answer;
        }
        let status = native(&self.query(target, &[])?);
        if status == errSecItemNotFound {
            return Ok(());
        }
        succeeded(status, CredentialStoreError::Delete)
    }
}

/// First light's answer to the legacy check, latched for the process by its first check.
#[cfg(feature = "first-light")]
static LEGACY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// First light's replacement for the vault's legacy deletion pass: every name earlier builds
/// wrote, from `account_targets`, checked by `status` alone and never read or deleted, each only
/// once `live` still says yes. Only not-found is absence; a present item, any other answer or a
/// no from `live` refuses, and the first check's answer is final.
#[cfg(feature = "first-light")]
pub(crate) fn legacy_absent(
    latch: &std::sync::OnceLock<bool>,
    live: &mut dyn FnMut() -> bool,
    mut status: impl FnMut(&str) -> Result<i32, CredentialStoreError>,
) -> Result<(), CredentialStoreError> {
    use kumochi_protocol::constants::account_targets;

    let absent = *latch.get_or_init(|| {
        account_targets(CREDENTIAL_SERVICE)
            .all(|name| live() && matches!(status(&name), Ok(code) if code == errSecItemNotFound))
    });
    if absent {
        Ok(())
    } else {
        Err(CredentialStoreError::Legacy)
    }
}

/// A legacy name's delete under first light: never made, and answered by the latched check, so
/// the vault's legacy pass mutates nothing. Any other name is `None`, deleted as usual.
#[cfg(feature = "first-light")]
fn legacy_delete(
    latch: &std::sync::OnceLock<bool>,
    target: &str,
) -> Option<Result<(), CredentialStoreError>> {
    use kumochi_protocol::constants::account_targets;

    account_targets(CREDENTIAL_SERVICE)
        .any(|name| name == target)
        .then(|| match latch.get() {
            Some(true) => Ok(()),
            _ => Err(CredentialStoreError::Legacy),
        })
}

fn validate(target: &str) -> Result<(), CredentialStoreError> {
    if target.is_empty() || target.contains('\0') || target.encode_utf16().count() > MAX_TARGET {
        return Err(CredentialStoreError::InvalidTarget);
    }
    Ok(())
}

/// Only not-found is absence; every other refusal stays a read error.
fn found(status: i32) -> Result<bool, CredentialStoreError> {
    if status == errSecItemNotFound {
        return Ok(false);
    }
    succeeded(status, CredentialStoreError::Read).map(|()| true)
}

fn succeeded(
    status: i32,
    error: fn(i32) -> CredentialStoreError,
) -> Result<(), CredentialStoreError> {
    if status == errSecSuccess {
        Ok(())
    } else {
        Err(error(status))
    }
}

/// A zeroizing copy of `bytes` owned by the returned view: CoreFoundation releases the view's
/// allocator only after the last reference to the view, and that release wipes and frees it.
fn owned_view(bytes: &[u8]) -> Result<CFRetained<CFData>, CredentialStoreError> {
    let info = Box::into_raw(Box::new(Zeroizing::new(bytes.to_vec())));
    // SAFETY: `info` is this function's live, unaliased Box; the pointer is taken after the move
    // so the view's bytes derive from the allocation the allocator will own.
    let backing = unsafe { &*info };
    let start = backing.as_ptr();
    let len = CFIndex::try_from(backing.len()).expect("the bounded blob length fits CFIndex");
    let mut context = CFAllocatorContext {
        version: 0,
        info: info.cast(),
        retain: None,
        release: Some(release_backing),
        copyDescription: None,
        allocate: Some(allocate_nothing),
        reallocate: None,
        deallocate: Some(deallocate_nothing),
        preferredSize: None,
    };
    // SAFETY: the context is fully initialized; the allocator copies it and owns `info`, which
    // its release callback frees once.
    let Some(owner) = (unsafe { CFAllocator::new(None, &raw mut context) }) else {
        // SAFETY: no allocator was created, so `info` is still this function's Box.
        drop(unsafe { Box::from_raw(info) });
        return Err(CredentialStoreError::Write(errSecAllocate));
    };
    // SAFETY: the bytes live until `owner` is released, and the view retains `owner` for as long
    // as the view exists.
    unsafe { CFData::with_bytes_no_copy(None, start, len, Some(&owner)) }
        .ok_or(CredentialStoreError::Write(errSecAllocate))
}

/// Frees the view's backing when its allocator is released: the only owner of that Box.
unsafe extern "C-unwind" fn release_backing(info: *const c_void) {
    // SAFETY: `info` is the Box `owned_view` gave this allocator, released exactly once.
    drop(unsafe { Box::from_raw(info.cast_mut().cast::<Zeroizing<Vec<u8>>>()) });
}

/// The view's allocator never allocates; it only owns the backing.
unsafe extern "C-unwind" fn allocate_nothing(
    _: CFIndex,
    _: CFOptionFlags,
    _: *mut c_void,
) -> *mut c_void {
    ptr::null_mut()
}

/// The backing is freed by the release callback, not per deallocation.
unsafe extern "C-unwind" fn deallocate_nothing(_: *mut c_void, _: *mut c_void) {}

/// An update changes the data alone, never the item's key or protection.
fn update_attributes(data: &CFData) -> Query {
    CFDictionary::from_slices(&[Keys::get().value_data], &[data.as_ref()])
}

/// A fresh `LAContext` that refuses any authentication UI; refused setup refuses the operation.
fn no_ui_context() -> Result<Retained<AnyObject>, CredentialStoreError> {
    let class = AnyClass::get(c"LAContext").ok_or(CredentialStoreError::Unsupported)?;
    // SAFETY: +new on LAContext takes no arguments and returns an owned object or nil.
    let context: Option<Retained<AnyObject>> = unsafe { msg_send![class, new] };
    let context = context.ok_or(CredentialStoreError::Unsupported)?;
    // SAFETY: interactionNotAllowed is a BOOL property of LAContext since macOS 10.13.
    let () = unsafe { msg_send![&context, setInteractionNotAllowed: true] };
    if !interaction_refused(&context) {
        return Err(CredentialStoreError::Unsupported);
    }
    Ok(context)
}

fn interaction_refused(context: &AnyObject) -> bool {
    // SAFETY: reads LAContext's BOOL interactionNotAllowed property.
    unsafe { msg_send![context, interactionNotAllowed] }
}

macro_rules! constant {
    ($name:ident) => {
        // SAFETY: reads one immutable Security framework constant that the loader initialized.
        unsafe { $name }
    };
}

/// The Security framework's query keys and values.
struct Keys {
    class: &'static CFString,
    generic_password: &'static CFString,
    group: &'static CFString,
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
        Self {
            class: constant!(kSecClass),
            generic_password: constant!(kSecClassGenericPassword),
            group: constant!(kSecAttrAccessGroup),
            service: constant!(kSecAttrService),
            account: constant!(kSecAttrAccount),
            data_protection: constant!(kSecUseDataProtectionKeychain),
            synchronizable: constant!(kSecAttrSynchronizable),
            context: constant!(kSecUseAuthenticationContext),
            accessible: constant!(kSecAttrAccessible),
            when_unlocked_this_device_only: constant!(kSecAttrAccessibleWhenUnlockedThisDeviceOnly),
            value_data: constant!(kSecValueData),
            return_data: constant!(kSecReturnData),
            match_limit: constant!(kSecMatchLimit),
            match_limit_one: constant!(kSecMatchLimitOne),
        }
    }
}

#[cfg(test)]
mod tests;
