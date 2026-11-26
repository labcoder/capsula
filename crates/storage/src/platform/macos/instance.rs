//! The instance capability's data protection Keychain item: its exact selector, per-role
//! entitlement admission, decoding, status mapping and the one read. Only the brain writes it.

use std::ptr::{self, NonNull};

use kumochi_protocol::constants::{AppleIds, INSTANCE_ACCOUNT, INSTANCE_SERVICE, ProductIdentity};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFData, CFDictionary, CFRetained, CFString, CFType,
};
use objc2_security::{
    SecItemCopyMatching, SecTask, errSecInteractionNotAllowed, errSecItemNotFound,
    errSecMissingEntitlement, errSecSuccess, kSecAttrAccessControl, kSecAttrAccessGroup,
    kSecAttrAccessible, kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly, kSecAttrAccount,
    kSecAttrService, kSecAttrSynchronizable, kSecClass, kSecClassGenericPassword, kSecMatchLimit,
    kSecMatchLimitOne, kSecReturnAttributes, kSecReturnData, kSecUseAuthenticationUI,
    kSecUseDataProtectionKeychain, kSecValueData,
};

/// The capability's exact length.
pub const INSTANCE_LEN: usize = 32;

/// A CFString-keyed Security query or attribute dictionary.
pub type ItemQuery = CFRetained<CFDictionary<CFString, CFType>>;

/// Which signed process asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The nested brain helper, the only writer.
    Brain,
    /// The window app, a reader.
    Shell,
}

/// The kind of call a status came from, since not-found means something different to each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// An exact copy.
    Read,
    /// An add or an update.
    Write,
    /// An exact delete.
    Delete,
}

/// Why an item call refused. It carries a status code at most.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ItemError {
    /// An exact read found no item.
    #[error("the instance item is missing")]
    Missing,
    /// An exact delete found no item.
    #[error("the instance item is already absent")]
    Absent,
    /// The item can't be used now; this says nothing about screen state.
    #[error("the instance item is unavailable")]
    Unavailable,
    /// This edition has no item, or this process isn't entitled to its group.
    #[error("this process isn't admitted to the instance item")]
    NotAdmitted,
    /// The item isn't one plain, protected 32-byte item.
    #[error("the instance item isn't the expected item")]
    Ineligible,
    /// Any other Security status.
    #[error("the instance item call failed (status {0})")]
    Failed(i32),
}

/// Maps a Security status from an `op` call.
///
/// # Errors
/// Every status but success.
pub fn status(op: Op, code: i32) -> Result<(), ItemError> {
    let not_found = code == errSecItemNotFound;
    Err(match code {
        _ if code == errSecSuccess => return Ok(()),
        _ if not_found && op == Op::Read => ItemError::Missing,
        _ if not_found && op == Op::Delete => ItemError::Absent,
        _ if code == errSecInteractionNotAllowed => ItemError::Unavailable,
        _ if code == errSecMissingEntitlement => ItemError::NotAdmitted,
        other => ItemError::Failed(other),
    })
}

/// This edition's identifiers. Only Dev has an item until release packaging provisions one; Gate
/// never maps to either.
///
/// # Errors
/// `NotAdmitted` for every other edition.
pub fn instance_edition(identity: ProductIdentity) -> Result<AppleIds, ItemError> {
    if identity == ProductIdentity::DEV {
        Ok(AppleIds::DEV)
    } else {
        Err(ItemError::NotAdmitted)
    }
}

/// The item as one admitted process sees it: its exact selector in the provisioned group.
pub struct InstanceItem {
    group: String,
}

impl std::fmt::Debug for InstanceItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InstanceItem(..)")
    }
}

impl InstanceItem {
    /// Admits this process as `role` from its own entitlements, before any item call. Whether the
    /// OS honours them is proved only by a signed run, never by this check.
    ///
    /// # Errors
    /// `NotAdmitted` for another edition or claims that don't name the instance group.
    pub fn admit(role: Role) -> Result<Self, ItemError> {
        let ids = instance_edition(ProductIdentity::ACTIVE)?;
        // SAFETY: creates an owned task object for this process with the default allocator.
        let task = unsafe { SecTask::from_self(None) }.ok_or(ItemError::NotAdmitted)?;
        let claim = |name: &'static str| {
            let name = CFString::from_static_str(name);
            // SAFETY: a null error pointer is allowed; the result is an owned value or none.
            unsafe { task.value_for_entitlement(&name, ptr::null_mut()) }
        };
        let app_id = claim("com.apple.application-identifier");
        let groups = claim("keychain-access-groups");
        let group = admitted_group(ids, role, app_id.as_deref(), groups.as_deref())?;
        Ok(Self { group })
    }

    /// `SecItemAdd`'s attributes: the selector, no UI, this device after first unlock, the value.
    #[must_use]
    pub fn add_attributes(&self, value: &CFData) -> ItemQuery {
        let k = keys();
        self.query(&[(k.accessible, k.protection.as_ref()), (k.value, value)])
    }

    /// The exact item, with no UI, for `SecItemUpdate`'s query and `SecItemDelete`.
    #[must_use]
    pub fn exact(&self) -> ItemQuery {
        self.query(&[])
    }

    /// `SecItemUpdate`'s changes: the value alone, never the item's key or protection.
    #[must_use]
    pub fn update_payload(value: &CFData) -> ItemQuery {
        CFDictionary::from_slices(&[keys().value], &[value.as_ref()])
    }

    /// Inspects an existing item's attributes, without its data.
    ///
    /// # Errors
    /// Status refusals, and `Ineligible` for anything but a plain protected item.
    pub fn inspect(&self) -> Result<(), ItemError> {
        let item = copy(&self.inspect_query())?;
        protection(&item).map(drop)
    }

    /// Reads the item back into `into`, which only a protected 32-byte value fills.
    ///
    /// # Errors
    /// Status refusals, and `Ineligible` for anything but that value.
    pub fn read(&self, into: &mut [u8; INSTANCE_LEN]) -> Result<(), ItemError> {
        let item = copy(&self.read_query())?;
        readback(&item, into)
    }

    pub(super) fn inspect_query(&self) -> ItemQuery {
        let k = keys();
        self.query(&[(k.return_attributes, k.yes), (k.match_limit, k.one)])
    }

    pub(super) fn read_query(&self) -> ItemQuery {
        let k = keys();
        self.query(&[
            (k.return_data, k.yes),
            (k.return_attributes, k.yes),
            (k.match_limit, k.one),
        ])
    }

    fn query(&self, extra: &[(&CFString, &CFType)]) -> ItemQuery {
        let k = keys();
        let group = CFString::from_str(&self.group);
        let service = CFString::from_static_str(INSTANCE_SERVICE);
        let account = CFString::from_static_str(INSTANCE_ACCOUNT);
        let mut pairs = vec![
            (k.class, k.generic_password.as_ref()),
            (k.group, group.as_ref()),
            (k.service, service.as_ref()),
            (k.account, account.as_ref()),
            (k.data_protection, k.yes),
            (k.synchronizable, k.no),
            (k.ui, k.ui_fail.as_ref()),
        ];
        pairs.extend_from_slice(extra);
        let (keys, values): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
        CFDictionary::from_slices(&keys, &values)
    }
}

fn copy(query: &ItemQuery) -> Result<CFRetained<CFType>, ItemError> {
    let mut result: *const CFType = ptr::null();
    // SAFETY: the query is a live CFString-keyed dictionary; result is a writable out pointer.
    let code = unsafe { SecItemCopyMatching(query.as_opaque(), &raw mut result) };
    status(Op::Read, code)?;
    let result = NonNull::new(result.cast_mut()).ok_or(ItemError::Ineligible)?;
    // SAFETY: a successful copy returns one owned reference, released once by this owner.
    Ok(unsafe { CFRetained::from_raw(result) })
}

/// The instance group `role` may use: its application identifier, split at its first dot, must be
/// exactly `P.<role's id>` with `P` non-empty and no wildcard, and its access groups an array of
/// strings that holds `P.<instance group>`.
fn admitted_group(
    ids: AppleIds,
    role: Role,
    app_id: Option<&CFType>,
    groups: Option<&CFType>,
) -> Result<String, ItemError> {
    let own = match role {
        Role::Brain => ids.brain,
        Role::Shell => ids.shell,
    };
    let app_id = app_id
        .and_then(CFType::downcast_ref::<CFString>)
        .ok_or(ItemError::NotAdmitted)?
        .to_string();
    let (prefix, id) = app_id.split_once('.').ok_or(ItemError::NotAdmitted)?;
    if prefix.is_empty() || prefix.contains('*') || id != own {
        return Err(ItemError::NotAdmitted);
    }
    let group = format!("{prefix}.{}", ids.instance_group);
    let groups = groups
        .and_then(CFType::downcast_ref::<CFArray>)
        .ok_or(ItemError::NotAdmitted)?;
    // SAFETY: callers pass only this process's entitlement value, a property list whose arrays
    // hold CoreFoundation objects alone, or a test's typed array of CoreFoundation objects.
    let groups = unsafe { groups.cast_unchecked::<CFType>() };
    let mut found = false;
    for entry in groups {
        let entry = entry
            .downcast_ref::<CFString>()
            .ok_or(ItemError::NotAdmitted)?;
        found |= entry.to_string() == group;
    }
    found.then_some(group).ok_or(ItemError::NotAdmitted)
}

/// The one protection check, for inspection and readback: exactly this device after first unlock,
/// and no access control.
pub(super) fn protection(item: &CFType) -> Result<&CFDictionary<CFString, CFType>, ItemError> {
    let item = item
        .downcast_ref::<CFDictionary>()
        .ok_or(ItemError::Ineligible)?;
    // SAFETY: Security returns attribute dictionaries keyed by CFString, with CoreFoundation values.
    let item = unsafe { item.cast_unchecked::<CFString, CFType>() };
    let k = keys();
    let protected = item
        .get(k.accessible)
        .is_some_and(|class| *class == *k.protection.as_ref());
    if !protected || item.contains_key(k.access_control) {
        return Err(ItemError::Ineligible);
    }
    Ok(item)
}

/// Copies a protected item's 32-byte value into `into`, and nothing on any refusal.
pub(super) fn readback(item: &CFType, into: &mut [u8; INSTANCE_LEN]) -> Result<(), ItemError> {
    let value = protection(item)?
        .get(keys().value)
        .and_then(|value| value.downcast::<CFData>().ok())
        .ok_or(ItemError::Ineligible)?;
    // SAFETY: this immutable CFData is retained for as long as the slice is read.
    let bytes = unsafe { value.as_bytes_unchecked() };
    let bytes = <&[u8; INSTANCE_LEN]>::try_from(bytes).map_err(|_| ItemError::Ineligible)?;
    into.copy_from_slice(bytes);
    Ok(())
}

macro_rules! constant {
    ($name:path) => {
        // SAFETY: reads one immutable Security framework constant that the loader initialized.
        unsafe { $name }
    };
}

/// The Security framework's keys and values this item uses.
pub(super) struct Keys {
    pub(super) class: &'static CFString,
    pub(super) generic_password: &'static CFString,
    pub(super) group: &'static CFString,
    pub(super) service: &'static CFString,
    pub(super) account: &'static CFString,
    pub(super) data_protection: &'static CFString,
    pub(super) synchronizable: &'static CFString,
    pub(super) ui: &'static CFString,
    pub(super) ui_fail: &'static CFString,
    pub(super) accessible: &'static CFString,
    pub(super) protection: &'static CFString,
    pub(super) access_control: &'static CFString,
    pub(super) value: &'static CFString,
    pub(super) return_data: &'static CFString,
    pub(super) return_attributes: &'static CFString,
    pub(super) match_limit: &'static CFString,
    pub(super) one: &'static CFType,
    pub(super) yes: &'static CFType,
    pub(super) no: &'static CFType,
}

pub(super) fn keys() -> Keys {
    #[expect(
        deprecated,
        reason = "its replacement needs LocalAuthentication, which this item must not link"
    )]
    let ui_fail = constant!(objc2_security::kSecUseAuthenticationUIFail);
    Keys {
        class: constant!(kSecClass),
        generic_password: constant!(kSecClassGenericPassword),
        group: constant!(kSecAttrAccessGroup),
        service: constant!(kSecAttrService),
        account: constant!(kSecAttrAccount),
        data_protection: constant!(kSecUseDataProtectionKeychain),
        synchronizable: constant!(kSecAttrSynchronizable),
        ui: constant!(kSecUseAuthenticationUI),
        ui_fail,
        accessible: constant!(kSecAttrAccessible),
        protection: constant!(kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly),
        access_control: constant!(kSecAttrAccessControl),
        value: constant!(kSecValueData),
        return_data: constant!(kSecReturnData),
        return_attributes: constant!(kSecReturnAttributes),
        match_limit: constant!(kSecMatchLimit),
        one: constant!(kSecMatchLimitOne).as_ref(),
        yes: CFBoolean::new(true).as_ref(),
        no: CFBoolean::new(false).as_ref(),
    }
}

#[cfg(test)]
mod tests;
