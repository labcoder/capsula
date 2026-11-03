//! The current user's ROOT certificate store, read as owned public DER.

use std::ffi::CStr;
use std::io;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ptr::{self, NonNull};
use std::slice;

use windows_sys::Win32::Foundation::{CRYPT_E_NOT_FOUND, ERROR_NO_MORE_FILES, SetLastError};
use windows_sys::Win32::Security::Cryptography::{
    CERT_CONTEXT, CTL_USAGE, CertCloseStore, CertEnumCertificatesInStore,
    CertFreeCertificateContext, CertGetEnhancedKeyUsage, CertOpenSystemStoreW,
    CertVerifyTimeValidity, HCERTSTORE,
};

use crate::platform::{TrustRoots, TrustRootsError};

/// The server-authentication extended key usage.
const SERVER_AUTH: &CStr = c"1.3.6.1.5.5.7.3.1";
/// Roots kept at most; the rest are skipped. A real store holds about a hundred.
const MAX_ROOTS: usize = 1_024;
/// The largest root kept, in DER bytes. Real roots are under 3 KiB.
const MAX_DER: usize = 16 * 1024;
/// The largest key-usage list read, in bytes.
const MAX_USAGE: usize = 16 * 1024;

/// Reads every root that's valid now and allowed for server authentication from the current
/// user's ROOT store, which includes the roots the machine supplies. Nothing is written to it.
///
/// # Errors
/// Fails when the store can't be opened or walked to its end, or when no root is kept.
pub fn trust_roots() -> Result<TrustRoots, TrustRootsError> {
    let store = Store::open()?;
    let mut roots = TrustRoots {
        certificates: Vec::new(),
        skipped: 0,
    };
    let mut current = store.next(None)?;
    while let Some(context) = current {
        if roots.certificates.len() < MAX_ROOTS
            && let Some(der) = kept(&context)
        {
            roots.certificates.push(der);
        } else {
            roots.skipped += 1;
        }
        current = store.next(Some(context))?;
    }
    if roots.certificates.is_empty() {
        return Err(TrustRootsError::Empty);
    }
    Ok(roots)
}

/// The certificate's DER when it's valid now, allowed for server authentication and bounded.
fn kept(context: &Context<'_>) -> Option<Vec<u8>> {
    let certificate = context.get();
    if certificate.pCertInfo.is_null() || certificate.pbCertEncoded.is_null() {
        return None;
    }
    // SAFETY: the certificate info belongs to this live context; a null time means now.
    if unsafe { CertVerifyTimeValidity(ptr::null(), certificate.pCertInfo) } != 0 {
        return None;
    }
    if usage(context) != Usage::Allowed {
        return None;
    }
    let len = usize::try_from(certificate.cbCertEncoded)
        .ok()
        .filter(|len| (1..=MAX_DER).contains(len))?;
    // SAFETY: the encoded bytes belong to this live context, with a checked non-null pointer
    // and bounded length; bytes need no alignment.
    let der = unsafe { slice::from_raw_parts(certificate.pbCertEncoded, len) };
    Some(der.to_vec())
}

/// What a certificate's extended key usage allows, read with `CertGetEnhancedKeyUsage`.
fn usage(context: &Context<'_>) -> Usage {
    let mut size = 0;
    // SAFETY: the context is live; a null buffer asks only for the size, written to `size`.
    if unsafe { CertGetEnhancedKeyUsage(context.get(), 0, ptr::null_mut(), &raw mut size) } == 0 {
        return Usage::Unreadable;
    }
    let Some(len) = usize::try_from(size)
        .ok()
        .filter(|len| (size_of::<CTL_USAGE>()..=MAX_USAGE).contains(len))
    else {
        return Usage::Unreadable;
    };
    // u64s keep the buffer aligned for the CTL_USAGE and pointers the OS writes into it.
    let mut buffer = vec![0_u64; len.div_ceil(size_of::<u64>())];
    // Cleared so a success with no identifiers reports only this call's last error.
    // SAFETY: setting the calling thread's last error touches no memory of ours.
    unsafe { SetLastError(0) };
    // SAFETY: the context is live, and the aligned buffer holds at least `size` writable bytes.
    let read = unsafe {
        CertGetEnhancedKeyUsage(context.get(), 0, buffer.as_mut_ptr().cast(), &raw mut size)
    };
    let code = last_error();
    if read == 0 {
        return classify(read, &[], code);
    }
    // SAFETY: the successful call wrote a CTL_USAGE at the start of this buffer, which is
    // aligned for it and at least its size.
    let header = unsafe { buffer.as_ptr().cast::<CTL_USAGE>().read() };
    // SAFETY: the buffer owns at least `len` initialized bytes for as long as this borrow.
    let bytes = unsafe { slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), len) };
    identifiers(
        bytes,
        header.rgpszUsageIdentifier.addr(),
        header.cUsageIdentifier,
    )
    .map_or(Usage::Unreadable, |identifiers| {
        classify(read, &identifiers, code)
    })
}

/// The usage identifiers, read only from inside `bytes`: the table at address `table` and every
/// string it points to must lie in the buffer. None when anything points outside it.
fn identifiers(bytes: &[u8], table: usize, count: u32) -> Option<Vec<&CStr>> {
    if count == 0 {
        return Some(Vec::new());
    }
    let base = bytes.as_ptr().addr();
    let width = size_of::<usize>();
    let start = table.checked_sub(base)?;
    let end = start.checked_add(usize::try_from(count).ok()?.checked_mul(width)?)?;
    bytes
        .get(start..end)?
        .chunks_exact(width)
        .map(|entry| {
            let address =
                usize::from_ne_bytes(entry.try_into().expect("entries are pointer-sized"));
            CStr::from_bytes_until_nul(bytes.get(address.checked_sub(base)?..)?).ok()
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Usage {
    Allowed,
    Excluded,
    Unreadable,
}

/// Classifies `CertGetEnhancedKeyUsage`'s result from its return value, its identifiers and
/// the last error captured right after it. Zero identifiers mean every use only with
/// `CRYPT_E_NOT_FOUND`; any other code can mean no use, so it's never read as every use.
fn classify(return_value: i32, identifiers: &[&CStr], last_error: i32) -> Usage {
    if return_value == 0 {
        return Usage::Unreadable;
    }
    if identifiers.is_empty() {
        return if last_error == CRYPT_E_NOT_FOUND {
            Usage::Allowed
        } else {
            Usage::Unreadable
        };
    }
    if identifiers.contains(&SERVER_AUTH) {
        Usage::Allowed
    } else {
        Usage::Excluded
    }
}

fn last_error() -> i32 {
    io::Error::last_os_error()
        .raw_os_error()
        .expect("a Win32 last error has a raw OS code")
}

/// An open system store, closed on drop after every context borrowed from it is freed.
struct Store(HCERTSTORE);

impl Store {
    fn open() -> Result<Self, TrustRootsError> {
        let name: Vec<u16> = "ROOT".encode_utf16().chain(Some(0)).collect();
        // SAFETY: provider zero is the default one; `name` is NUL-terminated and outlives the call.
        let store = unsafe { CertOpenSystemStoreW(0, name.as_ptr()) };
        if store.is_null() {
            return Err(TrustRootsError::Open(last_error()));
        }
        Ok(Self(store))
    }

    /// The certificate after `previous`, or the first one. `previous` moves into the call,
    /// which frees it even when it fails, so it's never freed here too.
    fn next<'store>(
        &'store self,
        previous: Option<Context<'store>>,
    ) -> Result<Option<Context<'store>>, TrustRootsError> {
        let previous = previous.map_or(ptr::null(), Context::into_raw);
        // SAFETY: the store is open, and `previous` is null or this store's context, now owned
        // by the call.
        let next = unsafe { CertEnumCertificatesInStore(self.0, previous) };
        if let Some(raw) = NonNull::new(next) {
            return Ok(Some(Context {
                raw,
                store: PhantomData,
            }));
        }
        let code = last_error();
        if code == CRYPT_E_NOT_FOUND || code == ERROR_NO_MORE_FILES.cast_signed() {
            Ok(None)
        } else {
            Err(TrustRootsError::Enumerate(code))
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // SAFETY: the store is open and closed exactly once; contexts borrow it, so they're gone.
        unsafe { CertCloseStore(self.0, 0) };
    }
}

/// A certificate context owned here: freed on drop unless handed back to the enumeration.
struct Context<'store> {
    raw: NonNull<CERT_CONTEXT>,
    store: PhantomData<&'store Store>,
}

impl Context<'_> {
    fn get(&self) -> &CERT_CONTEXT {
        // SAFETY: a non-null context from CertEnumCertificatesInStore, owned until drop or
        // `into_raw`.
        unsafe { self.raw.as_ref() }
    }

    fn into_raw(self) -> *const CERT_CONTEXT {
        ManuallyDrop::new(self).raw.as_ptr().cast_const()
    }
}

impl Drop for Context<'_> {
    fn drop(&mut self) {
        // SAFETY: this context is owned here and freed exactly once.
        unsafe { CertFreeCertificateContext(self.raw.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT_AUTH: &CStr = c"1.3.6.1.5.5.7.3.2";

    #[test]
    fn zero_identifiers_mean_every_use_only_after_success_with_not_found() {
        assert_eq!(classify(1, &[], CRYPT_E_NOT_FOUND), Usage::Allowed);
        assert_eq!(classify(0, &[], CRYPT_E_NOT_FOUND), Usage::Unreadable);
        for other in [0, 5, CRYPT_E_NOT_FOUND + 1] {
            assert_eq!(classify(1, &[], other), Usage::Unreadable, "{other:#x}");
            assert_eq!(classify(0, &[], other), Usage::Unreadable, "{other:#x}");
        }
    }

    #[test]
    fn a_usage_list_keeps_only_exact_server_auth() {
        assert_eq!(classify(1, &[CLIENT_AUTH, SERVER_AUTH], 0), Usage::Allowed);
        assert_eq!(
            classify(1, &[CLIENT_AUTH, c"1.3.6.1.5.5.7.3.1.1"], CRYPT_E_NOT_FOUND),
            Usage::Excluded
        );
    }

    #[test]
    fn identifiers_are_read_only_inside_the_buffer() {
        let width = size_of::<usize>();
        let mut bytes = vec![0_u8; 64];
        let base = bytes.as_ptr().addr();
        bytes[..width].copy_from_slice(&(base + 2 * width).to_ne_bytes());
        bytes[2 * width..][..18].copy_from_slice(b"1.3.6.1.5.5.7.3.1\0");
        assert_eq!(identifiers(&bytes, base, 1), Some(vec![SERVER_AUTH]));
        assert_eq!(identifiers(&bytes, 0, 0), Some(Vec::new()));
        assert_eq!(
            identifiers(&bytes, base + 64 - width + 1, 1),
            None,
            "table overruns"
        );
        assert_eq!(
            identifiers(&bytes, base - width, 1),
            None,
            "table before the buffer"
        );
        bytes[..width].copy_from_slice(&(base + 64).to_ne_bytes());
        assert_eq!(identifiers(&bytes, base, 1), None, "string past the end");
        bytes[..width].copy_from_slice(&(base + 60).to_ne_bytes());
        bytes[60..].fill(b'1');
        assert_eq!(identifiers(&bytes, base, 1), None, "string without a NUL");
    }

    #[test]
    fn the_current_user_root_store_yields_bounded_public_roots() {
        let roots = trust_roots().expect("the current user's ROOT store loads");
        assert!(!roots.certificates.is_empty(), "at least one root is kept");
        assert!(roots.certificates.len() <= MAX_ROOTS, "the cap holds");
        assert!(
            roots
                .certificates
                .iter()
                .all(|der| der.len() <= MAX_DER && der.first() == Some(&0x30)),
            "each root is bounded DER that starts with a SEQUENCE"
        );
    }
}
