//! The current user's ROOT certificate store, read as owned public DER.

use std::ffi::CStr;
use std::io;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ptr::{self, NonNull};
use std::slice;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{
    CRYPT_E_NOT_FOUND, ERROR_NO_MORE_FILES, FILETIME, SetLastError,
};
use windows_sys::Win32::Security::Cryptography::{
    CERT_CONTEXT, CERT_DISALLOWED_FILETIME_PROP_ID, CERT_FIND_EXISTING,
    CERT_STORE_OPEN_EXISTING_FLAG, CERT_STORE_PROV_SYSTEM_W, CERT_STORE_READONLY_FLAG,
    CERT_SYSTEM_STORE_CURRENT_USER, CTL_USAGE, CertCloseStore, CertEnumCertificatesInStore,
    CertFindCertificateInStore, CertFreeCertificateContext, CertGetCertificateContextProperty,
    CertGetEnhancedKeyUsage, CertOpenStore, CertVerifyTimeValidity, HCERTSTORE,
    PKCS_7_ASN_ENCODING, X509_ASN_ENCODING,
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
/// A FILETIME property's size in bytes.
const FILETIME_SIZE: u32 = 8;
/// The Unix epoch in FILETIME units: 100 ns intervals since 1601.
const UNIX_EPOCH_FILETIME: u64 = 116_444_736_000_000_000;

/// Reads valid server-auth roots, excluding Disallowed matches and due root-disallowed times,
/// from the current user's ROOT store, which includes the roots the machine supplies.
/// Both stores are opened read-only and only if they exist, so nothing is created or written.
///
/// # Errors
/// Fails when a store can't be opened or walked to its end, or when no root is kept.
pub fn trust_roots() -> Result<TrustRoots, TrustRootsError> {
    let store = Store::open("ROOT")?;
    let disallowed = Store::open("Disallowed")?;
    let now = filetime(SystemTime::now().duration_since(UNIX_EPOCH).ok());
    let mut roots = TrustRoots {
        certificates: Vec::new(),
        skipped: 0,
        unreadable: 0,
    };
    let mut current = store.next(None)?;
    while let Some(context) = current {
        let outcome = if roots.certificates.len() < MAX_ROOTS {
            kept(&context, &disallowed, now)
        } else {
            Err(Skip::Filtered)
        };
        count(&mut roots, outcome);
        current = store.next(Some(context))?;
    }
    if roots.certificates.is_empty() {
        return Err(TrustRootsError::Empty);
    }
    Ok(roots)
}

/// Why a root isn't kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Skip {
    /// Not valid now, not for server authentication, distrusted, oversized or past the cap.
    Filtered,
    /// Its validity, key usage or distrust data couldn't be read.
    Unreadable,
}

/// Adds one root's outcome to the counts.
fn count(roots: &mut TrustRoots, outcome: Result<Vec<u8>, Skip>) {
    match outcome {
        Ok(der) => roots.certificates.push(der),
        Err(skip) => {
            roots.skipped += 1;
            if skip == Skip::Unreadable {
                roots.unreadable += 1;
            }
        }
    }
}

/// The certificate's DER when it's admitted and not distrusted by Windows.
fn kept(context: &Context<'_>, disallowed: &Store, now: u64) -> Result<Vec<u8>, Skip> {
    let der = admitted(context)?;
    distrust(disallowed.contains(context), disallowed_after(context), now)?;
    Ok(der)
}

/// The certificate's DER when it's valid now, allowed for server authentication and bounded.
fn admitted(context: &Context<'_>) -> Result<Vec<u8>, Skip> {
    let certificate = context.get();
    if certificate.pCertInfo.is_null() || certificate.pbCertEncoded.is_null() {
        return Err(Skip::Unreadable);
    }
    // SAFETY: the certificate info belongs to this live context; a null time means now.
    if unsafe { CertVerifyTimeValidity(ptr::null(), certificate.pCertInfo) } != 0 {
        return Err(Skip::Filtered);
    }
    usage(context).admits()?;
    let len = usize::try_from(certificate.cbCertEncoded)
        .ok()
        .filter(|len| (1..=MAX_DER).contains(len))
        .ok_or(Skip::Filtered)?;
    // SAFETY: the encoded bytes belong to this live context, with a checked non-null pointer
    // and bounded length; bytes need no alignment.
    let der = unsafe { slice::from_raw_parts(certificate.pbCertEncoded, len) };
    Ok(der.to_vec())
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

impl Usage {
    /// Whether a root with this usage is kept, and if not, why.
    fn admits(self) -> Result<(), Skip> {
        match self {
            Self::Allowed => Ok(()),
            Self::Excluded => Err(Skip::Filtered),
            Self::Unreadable => Err(Skip::Unreadable),
        }
    }
}

/// Classifies `CertGetEnhancedKeyUsage`'s result from its return value, its identifiers and
/// the last error captured right after it. Zero identifiers mean every use only with
/// `CRYPT_E_NOT_FOUND`; any other code can mean no use, so it's never read as every use.
fn classify(return_value: i32, identifiers: &[&CStr], last_error: i32) -> Usage {
    if return_value == 0 {
        return Usage::Unreadable;
    }
    if identifiers.is_empty() {
        return match last_error {
            CRYPT_E_NOT_FOUND => Usage::Allowed,
            0 => Usage::Excluded,
            _ => Usage::Unreadable,
        };
    }
    if identifiers.contains(&SERVER_AUTH) {
        Usage::Allowed
    } else {
        Usage::Excluded
    }
}

/// What a lookup of a root's distrust data found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe<T> {
    Absent,
    Present(T),
    Unreadable,
}

/// A failed lookup: absent only when Windows says not found.
fn failed<T>(last_error: i32) -> Probe<T> {
    if last_error == CRYPT_E_NOT_FOUND {
        Probe::Absent
    } else {
        Probe::Unreadable
    }
}

/// Whether Windows distrusts a root: it's in the Disallowed store, or disallowed from a time at
/// or before `now`. Known distrust wins over an unreadable lookup, which otherwise fails closed.
fn distrust(listed: Probe<()>, disallowed_after: Probe<u64>, now: u64) -> Result<(), Skip> {
    if listed == Probe::Present(())
        || matches!(disallowed_after, Probe::Present(after) if after <= now)
    {
        return Err(Skip::Filtered);
    }
    if listed == Probe::Unreadable || disallowed_after == Probe::Unreadable {
        return Err(Skip::Unreadable);
    }
    Ok(())
}

/// Now in FILETIME units. A clock before 1970 counts as the far future, so all distrust applies.
fn filetime(since_unix: Option<Duration>) -> u64 {
    since_unix
        .and_then(|elapsed| u64::try_from(elapsed.as_nanos() / 100).ok())
        .and_then(|ticks| ticks.checked_add(UNIX_EPOCH_FILETIME))
        .unwrap_or(u64::MAX)
}

/// The time from which Windows disallows this root, in FILETIME units.
fn disallowed_after(context: &Context<'_>) -> Probe<u64> {
    let mut time = FILETIME::default();
    let mut size = FILETIME_SIZE;
    // SAFETY: the context is live, and `time` is a writable FILETIME of `size` bytes.
    let read = unsafe {
        CertGetCertificateContextProperty(
            context.get(),
            CERT_DISALLOWED_FILETIME_PROP_ID,
            (&raw mut time).cast(),
            &raw mut size,
        )
    };
    let code = last_error();
    if read == 0 {
        return failed(code);
    }
    disallowed_time(size, time)
}

/// A disallowed time Windows returned as `size` bytes.
fn disallowed_time(size: u32, time: FILETIME) -> Probe<u64> {
    match size {
        // Undocumented: Windows stores it empty for roots it distrusts from the start.
        0 => Probe::Present(0),
        FILETIME_SIZE => {
            Probe::Present((u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime))
        }
        _ => Probe::Unreadable,
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
    fn open(name: &str) -> Result<Self, TrustRootsError> {
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: the system provider reads `name` as a NUL-terminated wide string that outlives
        // the call; the flags open an existing store read-only, so nothing is created or written.
        let store = unsafe {
            CertOpenStore(
                CERT_STORE_PROV_SYSTEM_W,
                0,
                0,
                CERT_SYSTEM_STORE_CURRENT_USER
                    | CERT_STORE_READONLY_FLAG
                    | CERT_STORE_OPEN_EXISTING_FLAG,
                name.as_ptr().cast(),
            )
        };
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

impl Store {
    /// Whether this store holds a certificate identical to `context`.
    fn contains(&self, context: &Context<'_>) -> Probe<()> {
        // SAFETY: the store and `context` are live; CERT_FIND_EXISTING only reads `context`.
        let found = unsafe {
            CertFindCertificateInStore(
                self.0,
                X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
                0,
                CERT_FIND_EXISTING,
                ptr::from_ref(context.get()).cast(),
                ptr::null(),
            )
        };
        let code = last_error();
        let Some(raw) = NonNull::new(found) else {
            return failed(code);
        };
        // The match is a new context of this store, freed as it drops.
        drop(Context {
            raw,
            store: PhantomData,
        });
        Probe::Present(())
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
        assert_eq!(classify(1, &[], 0), Usage::Excluded, "no valid uses");
        for other in [5, CRYPT_E_NOT_FOUND + 1] {
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
    fn only_unreadable_usage_counts_as_unreadable() {
        assert_eq!(classify(1, &[], CRYPT_E_NOT_FOUND).admits(), Ok(()));
        assert_eq!(classify(1, &[SERVER_AUTH], 0).admits(), Ok(()));
        assert_eq!(classify(1, &[CLIENT_AUTH], 0).admits(), Err(Skip::Filtered));
        assert_eq!(
            classify(0, &[], CRYPT_E_NOT_FOUND).admits(),
            Err(Skip::Unreadable)
        );
        assert_eq!(classify(1, &[], 0).admits(), Err(Skip::Filtered));
        assert_eq!(classify(1, &[], 5).admits(), Err(Skip::Unreadable));
    }

    #[test]
    fn unreadable_roots_are_counted_apart_from_filtered_ones() {
        let mut roots = TrustRoots {
            certificates: Vec::new(),
            skipped: 0,
            unreadable: 0,
        };
        for outcome in [
            Ok(vec![0x30, 0x00]),
            Err(Skip::Filtered),
            Err(Skip::Unreadable),
            Err(Skip::Filtered),
            Err(Skip::Unreadable),
        ] {
            count(&mut roots, outcome);
        }
        assert_eq!(roots.certificates, [vec![0x30, 0x00]]);
        assert_eq!((roots.skipped, roots.unreadable), (4, 2));
    }

    #[test]
    fn distrusted_roots_are_filtered_and_unreadable_distrust_fails_closed() {
        use Probe::{Absent, Present, Unreadable};
        let now = 1_000;
        assert_eq!(distrust(Absent, Absent, now), Ok(()));
        assert_eq!(distrust(Absent, Present(now + 1), now), Ok(()));
        assert_eq!(distrust(Absent, Present(now), now), Err(Skip::Filtered));
        assert_eq!(distrust(Absent, Present(0), now), Err(Skip::Filtered));
        assert_eq!(distrust(Present(()), Absent, now), Err(Skip::Filtered));
        assert_eq!(distrust(Present(()), Unreadable, now), Err(Skip::Filtered));
        assert_eq!(distrust(Unreadable, Absent, now), Err(Skip::Unreadable));
        assert_eq!(distrust(Absent, Unreadable, now), Err(Skip::Unreadable));
        assert_eq!(
            distrust(Unreadable, Present(now + 1), now),
            Err(Skip::Unreadable)
        );
    }

    #[test]
    fn only_not_found_means_no_distrust_data() {
        assert_eq!(failed::<()>(CRYPT_E_NOT_FOUND), Probe::Absent);
        for other in [0, 5, CRYPT_E_NOT_FOUND + 1] {
            assert_eq!(failed::<()>(other), Probe::Unreadable, "{other:#x}");
        }
    }

    #[test]
    fn an_empty_disallowed_time_means_distrusted_from_the_start() {
        let time = FILETIME {
            dwLowDateTime: 2,
            dwHighDateTime: 1,
        };
        assert_eq!(disallowed_time(0, time), Probe::Present(0));
        assert_eq!(
            disallowed_time(FILETIME_SIZE, time),
            Probe::Present((1 << 32) | 2)
        );
        for size in [1, 4, 7, 9, 16] {
            assert_eq!(
                disallowed_time(size, time),
                Probe::Unreadable,
                "{size} bytes"
            );
        }
    }

    #[test]
    fn now_is_counted_in_filetime_units_from_1601() {
        assert_eq!(filetime(Some(Duration::ZERO)), 116_444_736_000_000_000);
        // 2000-01-01T00:00:00Z, a published FILETIME value.
        assert_eq!(
            filetime(Some(Duration::from_hours(262_968))),
            125_911_584_000_000_000
        );
        assert_eq!(filetime(None), u64::MAX);
        assert_eq!(filetime(Some(Duration::MAX)), u64::MAX);
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
    fn no_kept_root_is_distrusted_by_windows() {
        let store = Store::open("ROOT").expect("the current user's ROOT store opens");
        let disallowed = Store::open("Disallowed").expect("the Disallowed store opens");
        let now = filetime(SystemTime::now().duration_since(UNIX_EPOCH).ok());
        let (mut checked, mut kept_roots, mut due, mut otherwise_kept, mut kept_due) =
            (0, 0, 0, 0, 0);
        let mut current = store.next(None).expect("the ROOT store walks");
        while let Some(context) = current {
            checked += 1;
            let distrusted = disallowed.contains(&context) == Probe::Present(())
                || matches!(disallowed_after(&context), Probe::Present(after) if after <= now);
            due += usize::from(distrusted);
            otherwise_kept += usize::from(distrusted && admitted(&context).is_ok());
            if kept(&context, &disallowed, now).is_ok() {
                kept_roots += 1;
                kept_due += usize::from(distrusted);
            }
            current = store.next(Some(context)).expect("the ROOT store walks");
        }
        println!(
            "trust roots distrust: checked {checked}, kept {kept_roots}, distrusted {due}, \
             distrusted but otherwise kept {otherwise_kept}, kept and distrusted {kept_due}"
        );
        assert_eq!(kept_due, 0, "no kept root is distrusted by Windows");
    }

    #[test]
    fn the_current_user_root_store_yields_bounded_public_roots() {
        let roots = trust_roots().expect("the current user's ROOT store loads");
        println!(
            "trust roots: kept {}, skipped {}, unreadable {}",
            roots.certificates.len(),
            roots.skipped,
            roots.unreadable
        );
        assert_eq!(
            roots.unreadable, 0,
            "a healthy store has no unreadable root"
        );
        assert!(
            roots.certificates.len() >= 20,
            "a real store keeps at least 20 roots"
        );
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
