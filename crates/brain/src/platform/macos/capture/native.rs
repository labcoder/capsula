//! The native side: Quartz for the display layout, the grant query and the drawing, and
//! `ScreenCaptureKit` by message sends, its objects created and dropped inside its callbacks.

use std::ffi::{CStr, c_void};
use std::ptr::NonNull;
use std::sync::Mutex;

use block2::RcBlock;
use objc2::msg_send;
use objc2::rc::{Allocated, Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBlendMode, CGColorSpace, CGContext, CGDirectDisplayID,
    CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayIsInMirrorSet, CGDisplayMode, CGError,
    CGGetActiveDisplayList, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo,
    CGPreflightScreenCaptureAccess, kCGColorSpaceSRGB,
};

use super::chain::{Done, Ended, Request, Sck};
use super::layout::{Screen, whole};
use crate::screen::{Failure, Refusal};

/// `kCVPixelFormatType_32BGRA`.
const BGRA: u32 = u32::from_be_bytes(*b"BGRA");
/// The most displays read; a system with this many or more refuses `Layout`.
const MAX_DISPLAYS: usize = 16;
/// A native object the capability check vouched for was missing, or nil came back.
const MISSING: isize = -1;
/// The image isn't 8-bit, 32-bit-pixel colour, or no bitmap context could be made.
const UNDRAWABLE: i32 = -4;

/// `SCStreamErrorDomain`, a constant string the framework keeps for the process's life.
#[derive(Clone, Copy)]
pub(super) struct Domain(NonNull<AnyObject>);

// SAFETY: The constant is an immutable string that's never freed, so any thread may compare it.
unsafe impl Send for Domain {}
// SAFETY: As above; nothing mutates it.
unsafe impl Sync for Domain {}

impl Domain {
    /// Wraps the address of the framework's `NSString *const SCStreamErrorDomain`.
    ///
    /// # Safety
    /// `symbol` must be that exported constant's address in a framework that stays loaded.
    pub(super) unsafe fn read(symbol: NonNull<*mut AnyObject>) -> Option<Self> {
        // SAFETY: The caller vouches the address holds the constant's object pointer.
        NonNull::new(unsafe { symbol.read() }).map(Self)
    }
}

pub(super) struct Native {
    domain: Domain,
}

impl Native {
    pub(super) fn new(domain: Domain) -> Self {
        Self { domain }
    }
}

/// The filter and configuration for one screenshot.
pub(super) struct Shot {
    filter: Retained<AnyObject>,
    config: Retained<AnyObject>,
}

impl Sck for Native {
    type Content = Retained<AnyObject>;
    type Shot = Shot;
    type Image = CFRetained<CGImage>;

    fn displays(&self) -> Result<Vec<Screen>, Failure> {
        displays()
    }

    fn preflight(&self) -> bool {
        CGPreflightScreenCaptureAccess()
    }

    fn query(
        &self,
        admit: &dyn Fn() -> bool,
        ready: Done<Retained<AnyObject>>,
    ) -> Result<(), Ended> {
        let class = class(c"SCShareableContent")?;
        let domain = self.domain;
        let ready = Mutex::new(Some(ready));
        let block = RcBlock::new(move |content: *mut AnyObject, error: *mut AnyObject| {
            autoreleasepool(|_| {
                let Some(ready) = ready.lock().ok().and_then(|mut ready| ready.take()) else {
                    return;
                };
                // SAFETY: The framework passes this call's content or nil.
                let content = unsafe { Retained::retain(content) };
                ready(content.ok_or_else(|| ended(error, domain)));
            });
        });
        if !admit() {
            return Err(Ended::Stopped);
        }
        autoreleasepool(|_| {
            // SAFETY: The class method takes two BOOLs and a block of (content, error).
            unsafe {
                let () = msg_send![
                    class,
                    getShareableContentExcludingDesktopWindows: Bool::YES,
                    onScreenWindowsOnly: Bool::YES,
                    completionHandler: &*block
                ];
            }
        });
        Ok(())
    }

    fn prepare(&self, content: Retained<AnyObject>, request: &Request) -> Result<Shot, Ended> {
        let changed = Ended::Refused(Refusal::DisplayChanged);
        let excluded = Ended::Refused(Refusal::Excluded);
        // SAFETY: `displays` is the content's NSArray of `SCDisplay`.
        let displays = objects(unsafe { msg_send![&*content, displays] })?;
        // SAFETY: `applications` is the content's NSArray of `SCRunningApplication`.
        let apps = objects(unsafe { msg_send![&*content, applications] }).map_err(|_| excluded)?;
        drop(content);
        let display = the_one(displays.into_iter().filter(|display| {
            // SAFETY: `SCDisplay`'s `displayID` is a CGDirectDisplayID.
            let id: CGDirectDisplayID = unsafe { msg_send![&**display, displayID] };
            id == request.display
        }))
        .ok_or(changed)?;
        // SAFETY: `SCDisplay`'s `frame` is its CGRect in global points.
        let frame: CGRect = unsafe { msg_send![&*display, frame] };
        let at = [
            frame.origin.x,
            frame.origin.y,
            frame.size.width,
            frame.size.height,
        ]
        .map(whole);
        let [x, y] = request.origin.map(|side| Some(i64::from(side)));
        let [width, height] = request.points.map(|side| Some(i64::from(side)));
        if at != [x, y, width, height] {
            return Err(changed);
        }
        let shell = the_one(apps.into_iter().filter(|app| {
            // SAFETY: `SCRunningApplication`'s `processID` is a pid_t.
            let pid: libc::pid_t = unsafe { msg_send![&**app, processID] };
            u32::try_from(pid).is_ok_and(|pid| pid == request.shell)
        }))
        .ok_or(excluded)?;
        let array = class(c"NSArray")?;
        // SAFETY: `arrayWithObject:` makes an array of one object.
        let leave_out: Option<Retained<AnyObject>> =
            unsafe { msg_send![array, arrayWithObject: &*shell] };
        // SAFETY: `array` makes an empty array.
        let none: Option<Retained<AnyObject>> = unsafe { msg_send![array, array] };
        let (leave_out, none) = (leave_out.ok_or(excluded)?, none.ok_or(excluded)?);
        // SAFETY: Allocates a filter for the initializer below.
        let filter: Allocated<AnyObject> = unsafe { msg_send![class(c"SCContentFilter")?, alloc] };
        // SAFETY: The initializer takes a display, the applications to leave out and the windows
        // to keep despite that.
        let filter: Option<Retained<AnyObject>> = unsafe {
            msg_send![
                filter,
                initWithDisplay: &*display,
                excludingApplications: &*leave_out,
                exceptingWindows: &*none
            ]
        };
        let filter = filter.ok_or(excluded)?;
        // SAFETY: `new` makes a default stream configuration.
        let config: Option<Retained<AnyObject>> =
            unsafe { msg_send![class(c"SCStreamConfiguration")?, new] };
        let config = config.ok_or_else(missing)?;
        let [wide, high] = request.size.map(|side| usize::try_from(side).ok());
        let (wide, high) = (wide.ok_or_else(missing)?, high.ok_or_else(missing)?);
        // SAFETY: `setWidth:` takes the output width in pixels as size_t.
        unsafe { msg_send![&*config, setWidth: wide] }
        // SAFETY: `setHeight:` takes the output height in pixels as size_t.
        unsafe { msg_send![&*config, setHeight: high] }
        // SAFETY: `setShowsCursor:` takes a BOOL.
        unsafe { msg_send![&*config, setShowsCursor: Bool::YES] }
        // SAFETY: `setPixelFormat:` takes an OSType.
        unsafe { msg_send![&*config, setPixelFormat: BGRA] }
        Ok(Shot { filter, config })
    }

    fn shoot(
        &self,
        shot: Shot,
        commit: &dyn Fn() -> bool,
        done: Done<CFRetained<CGImage>>,
    ) -> Result<(), Ended> {
        let class = class(c"SCScreenshotManager")?;
        let domain = self.domain;
        let done = Mutex::new(Some(done));
        let block = RcBlock::new(move |image: *mut c_void, error: *mut AnyObject| {
            autoreleasepool(|_| {
                let Some(done) = done.lock().ok().and_then(|mut done| done.take()) else {
                    return;
                };
                done(match NonNull::new(image.cast::<CGImage>()) {
                    // SAFETY: The framework lends this call's image; retaining keeps it.
                    Some(image) => Ok(unsafe { CFRetained::retain(image) }),
                    None => Err(ended(error, domain)),
                });
            });
        });
        if !commit() {
            return Err(Ended::Stopped);
        }
        autoreleasepool(|_| {
            // SAFETY: The class method takes a filter, a configuration and a block of
            // (image, error).
            unsafe {
                let () = msg_send![
                    class,
                    captureImageWithFilter: &*shot.filter,
                    configuration: &*shot.config,
                    completionHandler: &*block
                ];
            }
        });
        Ok(())
    }

    fn draw(
        &self,
        image: &CFRetained<CGImage>,
        size: [u32; 2],
        frame: &mut [u8],
    ) -> Result<(), Failure> {
        draw(image, size, frame)
    }
}

/// The active displays, for a capture and for the seal's check.
///
/// # Errors
/// `Layout` when there may be more displays than it reads, or Quartz's error code.
pub(super) fn displays() -> Result<Vec<Screen>, Failure> {
    let mut ids = [0; MAX_DISPLAYS];
    let capacity = u32::try_from(MAX_DISPLAYS).unwrap_or(0);
    let mut count = 0_u32;
    // SAFETY: `ids` holds `capacity` writable entries and `count` one writable value.
    let status = unsafe { CGGetActiveDisplayList(capacity, ids.as_mut_ptr(), &raw mut count) };
    if status != CGError::Success {
        return Err(Failure::Fault(status.0));
    }
    let count = usize::try_from(count).unwrap_or(MAX_DISPLAYS);
    if count >= MAX_DISPLAYS {
        return Err(Refusal::Layout.into());
    }
    Ok(ids
        .iter()
        .take(count)
        .map(|&id| {
            let bounds = CGDisplayBounds(id);
            let mode = CGDisplayCopyDisplayMode(id);
            Screen {
                id,
                origin: [bounds.origin.x, bounds.origin.y],
                points: [bounds.size.width, bounds.size.height],
                pixels: [
                    CGDisplayMode::pixel_width(mode.as_deref()),
                    CGDisplayMode::pixel_height(mode.as_deref()),
                ],
                mirrored: CGDisplayIsInMirrorSet(id),
            }
        })
        .collect())
}

/// Clears `frame` and copies `image` over all of it as premultiplied BGRA, after checking the
/// image. Drawing reports nothing, so only these checks can refuse.
///
/// # Errors
/// `DisplayChanged` for another size, `Invalid` for a frame of the wrong length, or
/// `UNDRAWABLE` for an image that isn't 8-bit, 32-bit-pixel colour or a context that can't be made.
pub(super) fn draw(image: &CGImage, size: [u32; 2], frame: &mut [u8]) -> Result<(), Failure> {
    let [width, height] = size.map(|side| usize::try_from(side).unwrap_or(usize::MAX));
    if CGImage::width(Some(image)) != width || CGImage::height(Some(image)) != height {
        return Err(Refusal::DisplayChanged.into());
    }
    let alpha = CGImage::alpha_info(Some(image));
    if CGImage::bits_per_component(Some(image)) != 8
        || CGImage::bits_per_pixel(Some(image)) != 32
        || alpha == CGImageAlphaInfo::None
        || alpha == CGImageAlphaInfo::Only
    {
        return Err(Failure::Fault(UNDRAWABLE));
    }
    let stride = width.checked_mul(4).ok_or(Refusal::Invalid)?;
    if Some(frame.len()) != stride.checked_mul(height) {
        return Err(Refusal::Invalid.into());
    }
    // SAFETY: Reads the framework's constant colour space name.
    let srgb = unsafe { kCGColorSpaceSRGB };
    let space = CGColorSpace::with_name(Some(srgb)).ok_or(Failure::Fault(UNDRAWABLE))?;
    let info = CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0;
    // SAFETY: `frame` is `stride * height` writable bytes and outlives the context, dropped below.
    let context = unsafe {
        CGBitmapContextCreate(
            frame.as_mut_ptr().cast(),
            width,
            height,
            8,
            stride,
            Some(&space),
            info,
        )
    }
    .ok_or(Failure::Fault(UNDRAWABLE))?;
    let rect = CGRect::new(
        CGPoint::ZERO,
        CGSize::new(f64::from(size[0]), f64::from(size[1])),
    );
    CGContext::clear_rect(Some(&context), rect);
    CGContext::set_blend_mode(Some(&context), CGBlendMode::Copy);
    CGContext::draw_image(Some(&context), rect, Some(image));
    drop(context);
    Ok(())
}

/// The only item, or none for zero or several.
pub(super) fn the_one<T>(mut items: impl Iterator<Item = T>) -> Option<T> {
    let first = items.next()?;
    items.next().is_none().then_some(first)
}

fn missing() -> Ended {
    Ended::Code {
        sck: false,
        code: MISSING,
    }
}

fn class(name: &CStr) -> Result<&'static AnyClass, Ended> {
    AnyClass::get(name).ok_or_else(missing)
}

/// The objects of an `NSArray` of objects.
fn objects(array: Option<Retained<AnyObject>>) -> Result<Vec<Retained<AnyObject>>, Ended> {
    let array = array.ok_or_else(missing)?;
    // SAFETY: `count` is NSArray's NSUInteger length.
    let count: usize = unsafe { msg_send![&*array, count] };
    (0..count)
        .map(|index| {
            // SAFETY: `index` is below the array's count.
            let object: Option<Retained<AnyObject>> =
                unsafe { msg_send![&*array, objectAtIndex: index] };
            object.ok_or_else(missing)
        })
        .collect()
}

/// An error's domain and code; its text is never read.
fn ended(error: *mut AnyObject, domain: Domain) -> Ended {
    // SAFETY: The framework passes this call's error or nil.
    let Some(error) = (unsafe { Retained::retain(error) }) else {
        return missing();
    };
    // SAFETY: `code` is NSError's NSInteger.
    let code: isize = unsafe { msg_send![&*error, code] };
    // SAFETY: `domain` is NSError's string.
    let theirs: Option<Retained<AnyObject>> = unsafe { msg_send![&*error, domain] };
    let sck = theirs.is_some_and(|theirs| {
        // SAFETY: Both are strings, and `domain` lives for the process.
        let same: Bool = unsafe { msg_send![&*theirs, isEqualToString: domain.0.as_ptr()] };
        same.as_bool()
    });
    Ended::Code { sck, code }
}

#[cfg(test)]
mod tests;
