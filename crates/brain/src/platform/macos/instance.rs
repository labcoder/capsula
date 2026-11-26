//! The instance item's three writes on storage's admitted selector and dictionaries, and the
//! session custody lock in this edition's admitted product folder.

use std::ffi::c_void;
use std::fs::TryLockError;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::ptr;

use kumochi_protocol::constants::{DATA_DIR, ProductIdentity, SESSION_LOCK};
use kumochi_protocol::message::CAPABILITY_LEN;
use kumochi_storage::{
    AcquisitionError, AdmittedLock, ApplicationSupportError, InstanceItem, ItemError, Op,
    ProductFolder, Role, admit_support, application_support_dir, instance_edition, status,
};
use objc2_core_foundation::{
    CFAllocator, CFAllocatorContext, CFData, CFIndex, CFOptionFlags, CFRetained,
};
use objc2_security::{
    SecItemAdd, SecItemDelete, SecItemUpdate, errSecAllocate, errSecDuplicateItem,
};
use zeroize::Zeroize as _;

use crate::startup::custody::CustodyError;
use crate::startup::instance::{Added, InstanceStore};

/// The brain's admitted view of the item, its only writer.
#[derive(Debug)]
pub(crate) struct KeychainItem(InstanceItem);

impl KeychainItem {
    /// Admits this process as the brain from its own entitlements, before any item call.
    pub(crate) fn admit() -> Result<Self, ItemError> {
        InstanceItem::admit(Role::Brain).map(Self)
    }
}

impl InstanceStore for KeychainItem {
    type Error = ItemError;

    fn add(&self, key: &[u8; CAPABILITY_LEN]) -> Result<Added, ItemError> {
        let value = owned_view(key)?;
        let attributes = self.0.add_attributes(&value);
        // SAFETY: the attributes are a live CFString-keyed dictionary; no result is requested.
        let code = unsafe { SecItemAdd(attributes.as_opaque(), ptr::null_mut()) };
        if code == errSecDuplicateItem {
            return Ok(Added::Duplicate);
        }
        status(Op::Write, code).map(|()| Added::Fresh)
    }

    fn inspect(&self) -> Result<(), ItemError> {
        self.0.inspect()
    }

    fn update(&self, key: &[u8; CAPABILITY_LEN]) -> Result<(), ItemError> {
        let value = owned_view(key)?;
        let query = self.0.exact();
        let payload = InstanceItem::update_payload(&value);
        // SAFETY: the query and payload are live CFString-keyed dictionaries for this call.
        let code = unsafe { SecItemUpdate(query.as_opaque(), payload.as_opaque()) };
        status(Op::Write, code)
    }

    fn read(&self, into: &mut [u8; CAPABILITY_LEN]) -> Result<(), ItemError> {
        self.0.read(into)
    }

    fn delete(&self) -> Result<(), ItemError> {
        let query = self.0.exact();
        // SAFETY: the query is a live CFString-keyed dictionary naming one exact item.
        let code = unsafe { SecItemDelete(query.as_opaque()) };
        status(Op::Delete, code)
    }
}

/// Custody's lock, already taken, with the absolute product path its acquisition walked. Only
/// `lock_at` makes one, once the lock is held, so a lock never pairs with another folder.
pub(crate) struct InstanceLock {
    _lock: AdmittedLock,
    product: PathBuf,
    /// The device and inode of the product folder, read from the descriptor that created it;
    /// none when the folder was already there.
    // shortcut: held in memory only; first light records the root with `stat` around the run,
    // and D-b decides where a durable record lives before any run reopens a root.
    created: Option<(u64, u64)>,
}

impl InstanceLock {
    /// The product folder this lock was taken in, by the pathname the acquisition walked.
    pub(crate) fn product(&self) -> &Path {
        &self.product
    }

    /// Whether this acquisition created the product folder and its pathname, unfollowed, still
    /// names that folder. A same-user rename race after this check stays possible: the store
    /// opens by pathname.
    pub(crate) fn fresh(&self) -> bool {
        self.created.is_some_and(|created| {
            std::fs::symlink_metadata(&self.product)
                .is_ok_and(|now| now.is_dir() && (now.dev(), now.ino()) == created)
        })
    }
}

/// Custody in the OS-resolved Application Support folder.
pub(crate) fn instance_custody() -> Result<InstanceLock, CustodyError> {
    custody_in(ProductIdentity::ACTIVE, application_support_dir)
}

/// Refuses another edition before `support` resolves any folder, then locks in that folder.
fn custody_in(
    identity: ProductIdentity,
    support: impl FnOnce() -> Result<PathBuf, ApplicationSupportError>,
) -> Result<InstanceLock, CustodyError> {
    instance_edition(identity).map_err(|_| CustodyError::NotAdmitted)?;
    lock_at(&support().map_err(|_| CustodyError::Unavailable)?)
}

/// Refuses another edition first, then admits `support`, holds or privately creates this
/// edition's product folder, and locks the session leaf on the descriptor it opened. The held
/// lock carries the absolute product path this acquisition walked, so nothing resolves it again.
pub(crate) fn lock_at(support: &Path) -> Result<InstanceLock, CustodyError> {
    instance_edition(ProductIdentity::ACTIVE).map_err(|_| CustodyError::NotAdmitted)?;
    let (product, created) = match admit_support(support)
        .and_then(|support| support.product_folder(DATA_DIR))
        .map_err(refused)?
    {
        ProductFolder::Held(product) => (product, None),
        ProductFolder::Missing(support) => {
            let product = support.create_product(DATA_DIR).map_err(refused)?;
            let held = product
                .metadata()
                .map_err(|error| CustodyError::Open(error.kind()))?;
            (product, Some((held.dev(), held.ino())))
        }
    };
    let lock = product.into_lock(SESSION_LOCK).map_err(refused)?;
    lock.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => CustodyError::Busy,
        TryLockError::Error(error) => CustodyError::Open(error.kind()),
    })?;
    Ok(InstanceLock {
        _lock: lock,
        product: support.join(DATA_DIR),
        created,
    })
}

fn refused(error: AcquisitionError) -> CustodyError {
    match error {
        AcquisitionError::Open(kind) => CustodyError::Open(kind),
        AcquisitionError::Admission(_) => CustodyError::Untrusted,
    }
}

/// A view of a copy of `key`: CoreFoundation releases the view's allocator after the last
/// reference to the view, and that release wipes and frees the copy.
fn owned_view(key: &[u8; CAPABILITY_LEN]) -> Result<CFRetained<CFData>, ItemError> {
    let info = Box::into_raw(Box::new([0_u8; CAPABILITY_LEN]));
    let start = info.cast::<u8>();
    // SAFETY: `info` is this function's live, unaliased allocation of exactly CAPABILITY_LEN bytes.
    unsafe { start.copy_from_nonoverlapping(key.as_ptr(), CAPABILITY_LEN) };
    let len = CFIndex::try_from(CAPABILITY_LEN).expect("the key length fits CFIndex");
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
    // its release callback wipes and frees once.
    let Some(owner) = (unsafe { CFAllocator::new(None, &raw mut context) }) else {
        // SAFETY: no allocator took `info`, so this is its one release.
        unsafe { release_backing(info.cast()) };
        return Err(ItemError::Failed(errSecAllocate));
    };
    // SAFETY: the bytes live until `owner` is released, and the view retains `owner`.
    unsafe { CFData::with_bytes_no_copy(None, start, len, Some(&owner)) }
        .ok_or(ItemError::Failed(errSecAllocate))
}

#[cfg(test)]
thread_local! {
    /// How many view backings this thread released, and how many of those were all zero once
    /// wiped, for the tests.
    pub(super) static RELEASED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static WIPED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Wipes and frees the view's backing when its allocator is released: the only owner of that Box.
unsafe extern "C-unwind" fn release_backing(info: *const c_void) {
    // SAFETY: `info` is the Box `owned_view` made, released exactly once.
    let mut backing = unsafe { Box::from_raw(info.cast_mut().cast::<[u8; CAPABILITY_LEN]>()) };
    backing.zeroize();
    #[cfg(test)]
    if backing.iter().all(|byte| *byte == 0) {
        WIPED.set(WIPED.get() + 1);
    }
    drop(backing);
    #[cfg(test)]
    RELEASED.set(RELEASED.get() + 1);
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

#[cfg(test)]
pub(in crate::platform) mod tests;
