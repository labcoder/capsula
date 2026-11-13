//! Cached UI Automation, owned by the disposable MTA thread rather than the serve thread.

use std::marker::PhantomData;
use std::os::windows::io::{AsHandle as _, FromRawHandle as _, OwnedHandle};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use kumochi_protocol::geometry::{PhysicalRect, WindowRef};
use windows::Win32::Foundation::{
    E_ACCESSDENIED, ERROR_INVALID_PARAMETER, ERROR_INVALID_WINDOW_HANDLE, HWND, RECT,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationCacheRequest, IUIAutomationElement,
    IUIAutomationTreeWalker, TreeScope_Element, UIA_BoundingRectanglePropertyId,
    UIA_ControlTypePropertyId, UIA_E_ELEMENTNOTAVAILABLE, UIA_IsOffscreenPropertyId,
    UIA_IsPasswordPropertyId, UIA_NamePropertyId,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId,
    IsWindowVisible,
};
use windows::core::{HRESULT, Interface};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

use crate::screen::{
    CONNECTION_TIMEOUT, ElementPlatform, ElementReader, ElementSource, ElementTarget, LIMITS,
    NodeProps, SourceError, TRANSACTION_TIMEOUT, ThreadedReader, TopWindow, Wake, excluded_image,
};

pub(crate) fn screen_elements(wake: Wake) -> Result<Box<dyn ElementReader>, i32> {
    let shell = Arc::new(AtomicU32::new(0));
    let source_shell = Arc::clone(&shell);
    ThreadedReader::new(
        move || Automation::new(Arc::clone(&source_shell)).map_err(|e| e.code().0),
        Native(shell),
        wake,
    )
    .map(|reader| -> Box<dyn ElementReader> { Box::new(reader) })
}

#[derive(Clone)]
struct Native(Arc<AtomicU32>);

impl ElementPlatform for Native {
    fn foreground(&mut self) -> Option<TopWindow> {
        foreground()
    }
    fn set_shell_pid(&mut self, pid: u32) {
        self.0.store(pid, Ordering::Relaxed);
    }
    fn excluded(&mut self, pid: u32) -> Result<bool, SourceError> {
        excluded(pid, self.0.load(Ordering::Relaxed))
    }
}

/// Whether `pid` is Kumochi's: this worker, the connected `shell`, or a Kumochi image. The one
/// rule for both the element walk and the window under the pointer.
pub(super) fn excluded(pid: u32, shell: u32) -> Result<bool, SourceError> {
    if own_pid(pid, std::process::id(), shell) {
        return Ok(true);
    }
    // SAFETY: only query access is requested; Windows validates this process id.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return Err(io_error(std::io::Error::last_os_error()));
    }
    // SAFETY: this successful OpenProcess result is owned and closed exactly once.
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    let path = super::pipe::process_image(process.as_handle()).map_err(io_error)?;
    image_excluded(path.file_name().and_then(|name| name.to_str()))
}

fn own_pid(pid: u32, worker: u32, shell: u32) -> bool {
    pid != 0 && (pid == worker || pid == shell)
}

fn image_excluded(name: Option<&str>) -> Result<bool, SourceError> {
    name.map(excluded_image).ok_or(SourceError::Fault(-1))
}

fn mapped(code: HRESULT) -> SourceError {
    if code == E_ACCESSDENIED {
        SourceError::Elevated
    } else if [
        HRESULT(UIA_E_ELEMENTNOTAVAILABLE.cast_signed()),
        HRESULT::from_win32(ERROR_INVALID_WINDOW_HANDLE.0),
        HRESULT::from_win32(ERROR_INVALID_PARAMETER.0),
    ]
    .contains(&code)
    {
        SourceError::NoWindow
    } else {
        SourceError::Fault(code.0)
    }
}

fn error(error: windows::core::Error) -> SourceError {
    let code = error.code();
    drop(error);
    mapped(code)
}

fn io_error(error: std::io::Error) -> SourceError {
    let code = error.raw_os_error();
    drop(error);
    code.map_or(SourceError::Fault(-1), |code| {
        mapped(HRESULT::from_win32(code.unsigned_abs()))
    })
}

pub(super) fn rectangle(rect: RECT) -> PhysicalRect {
    PhysicalRect {
        origin: [rect.left, rect.top],
        size: [
            u32::try_from((i64::from(rect.right) - i64::from(rect.left)).max(0))
                .expect("two i32 coordinates differ by at most u32::MAX"),
            u32::try_from((i64::from(rect.bottom) - i64::from(rect.top)).max(0))
                .expect("two i32 coordinates differ by at most u32::MAX"),
        ],
    }
}

struct WindowOwner {
    root: bool,
    visible: bool,
    pid: u32,
}

impl WindowOwner {
    fn matches(&self, expected: u32) -> bool {
        self.root && self.visible && self.pid != 0 && self.pid == expected
    }
}

fn snapshot(window: HWND, expected: Option<u32>) -> Result<TopWindow, SourceError> {
    // SAFETY: this only queries the opaque handle; it is never dereferenced by Rust.
    let root = unsafe { GetAncestor(window, GA_ROOT) };
    // SAFETY: Windows validates the opaque handle and returns its visibility.
    let visible = unsafe { IsWindowVisible(window) }.as_bool();
    let mut actual = 0;
    // SAFETY: actual is writable; Windows validates the handle.
    unsafe { GetWindowThreadProcessId(window, Some(&raw mut actual)) };
    if !(WindowOwner {
        root: root == window,
        visible,
        pid: actual,
    })
    .matches(expected.unwrap_or(actual))
    {
        return Err(SourceError::NoWindow);
    }
    let mut rect = RECT::default();
    // SAFETY: rect is writable and the handle has just been validated.
    unsafe { GetWindowRect(window, &raw mut rect) }.map_err(error)?;
    Ok(TopWindow {
        window: WindowRef(u64::try_from(window.0.addr()).map_err(|_| SourceError::NoWindow)?),
        pid: actual,
        rect: rectangle(rect),
    })
}

pub(super) fn foreground() -> Option<TopWindow> {
    // SAFETY: the call returns an opaque handle and reads no window contents.
    let window = unsafe { GetForegroundWindow() };
    // SAFETY: Windows validates the opaque foreground handle, including null.
    let root = unsafe { GetAncestor(window, GA_ROOT) };
    snapshot(root, None).ok()
}

struct Apartment(PhantomData<Rc<()>>);
impl Apartment {
    fn enter() -> windows::core::Result<Self> {
        // SAFETY: this thread initializes its own MTA before creating COM objects.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        Ok(Self(PhantomData))
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: the guard is non-Send and balances this thread's successful initialization.
        unsafe { CoUninitialize() };
    }
}

struct Automation {
    client: IUIAutomation,
    cache: IUIAutomationCacheRequest,
    walker: IUIAutomationTreeWalker,
    shell: Arc<AtomicU32>,
    _apartment: Apartment,
}

impl Automation {
    fn new(shell: Arc<AtomicU32>) -> windows::core::Result<Self> {
        let apartment = Apartment::enter()?;
        // SAFETY: COM is initialized and the requested class/interface are documented UIA APIs.
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) }?;
        let timeouts: IUIAutomation2 = automation.cast()?;
        // SAFETY: the interface belongs to this initialized thread; the timeout is bounded.
        unsafe {
            timeouts.SetConnectionTimeout(
                u32::try_from(CONNECTION_TIMEOUT.as_millis()).expect("100 ms fits u32"),
            )
        }?;
        // SAFETY: the interface belongs to this initialized thread; the timeout is bounded.
        unsafe {
            timeouts.SetTransactionTimeout(
                u32::try_from(TRANSACTION_TIMEOUT.as_millis()).expect("100 ms fits u32"),
            )
        }?;
        // SAFETY: automation is live on its initialized thread.
        let cache = unsafe { automation.CreateCacheRequest() }?;
        // SAFETY: cache is live; only the current node is cached.
        unsafe { cache.SetTreeScope(TreeScope_Element) }?;
        for property in [
            UIA_NamePropertyId,
            UIA_ControlTypePropertyId,
            UIA_BoundingRectanglePropertyId,
            UIA_IsPasswordPropertyId,
            UIA_IsOffscreenPropertyId,
        ] {
            // SAFETY: cache is live and each property id is a documented UIA property.
            unsafe { cache.AddProperty(property) }?;
        }
        // SAFETY: automation is live; this walker filters the control view.
        let walker = unsafe { automation.ControlViewWalker() }?;
        Ok(Self {
            client: automation,
            cache,
            walker,
            shell,
            _apartment: apartment,
        })
    }
}

fn name(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied())
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .take(LIMITS.name_chars + 1)
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Visibility {
    Onscreen,
    Offscreen,
}

fn properties(
    mut props: NodeProps,
    visibility: Visibility,
    cached_name: impl FnOnce() -> Result<windows::core::BSTR, SourceError>,
) -> Result<NodeProps, SourceError> {
    props.name = if props.password || visibility == Visibility::Offscreen {
        String::new()
    } else {
        name(&cached_name()?)
    };
    Ok(props)
}

fn optional<T>(result: windows::core::Result<T>) -> Result<Option<T>, SourceError> {
    match result {
        Ok(node) => Ok(Some(node)),
        Err(e) if e.code().is_ok() => Ok(None),
        Err(e) => Err(error(e)),
    }
}

impl ElementSource for Automation {
    type Node = IUIAutomationElement;
    fn root(
        &mut self,
        target: ElementTarget,
    ) -> Result<(Self::Node, PhysicalRect, u32), SourceError> {
        let top = match target {
            ElementTarget::Foreground => foreground().ok_or(SourceError::NoWindow)?,
            ElementTarget::Window { window, pid } => {
                let handle = HWND(std::ptr::without_provenance_mut(
                    usize::try_from(window.0).map_err(|_| SourceError::NoWindow)?,
                ));
                snapshot(handle, Some(pid))?
            }
        };
        if Native(Arc::clone(&self.shell)).excluded(top.pid)? {
            return Err(SourceError::Excluded);
        }
        let handle = HWND(std::ptr::without_provenance_mut(
            usize::try_from(top.window.0).map_err(|_| SourceError::NoWindow)?,
        ));
        // SAFETY: the visible root and owner were validated; this builds one node's cache.
        let root = unsafe { self.client.ElementFromHandleBuildCache(handle, &self.cache) }
            .map_err(error)?;
        snapshot(handle, Some(top.pid))?;
        // SAFETY: BoundingRectangle was requested in this element's cache.
        let rect = unsafe { root.CachedBoundingRectangle() }.map_err(error)?;
        Ok((root, rectangle(rect), top.pid))
    }
    fn read(&mut self, node: &Self::Node) -> Result<NodeProps, SourceError> {
        let (password, rect) = self.mask(node)?;
        // SAFETY: IsOffscreen was requested in this element's cache.
        let offscreen = unsafe { node.CachedIsOffscreen() }
            .map_err(error)?
            .as_bool();
        // SAFETY: ControlType was requested in this element's cache.
        let control_type = unsafe { node.CachedControlType() }.map_err(error)?.0;
        properties(
            NodeProps {
                name: String::new(),
                password,
                control_type,
                rect,
            },
            if offscreen {
                Visibility::Offscreen
            } else {
                Visibility::Onscreen
            },
            || {
                // SAFETY: Name was cached; the closure is never called for private/offscreen nodes.
                unsafe { node.CachedName() }.map_err(error)
            },
        )
    }
    fn mask(&mut self, node: &Self::Node) -> Result<(bool, PhysicalRect), SourceError> {
        // SAFETY: IsPassword was requested in this element's cache.
        let password = unsafe { node.CachedIsPassword() }.map_err(error)?.as_bool();
        // SAFETY: BoundingRectangle was requested in this element's cache.
        let rect = unsafe { node.CachedBoundingRectangle() }.map_err(error)?;
        Ok((password, rectangle(rect)))
    }
    fn first_child(&mut self, node: &Self::Node) -> Result<Option<Self::Node>, SourceError> {
        // SAFETY: node, cache and walker are live on this initialized thread.
        optional(unsafe {
            self.walker
                .GetFirstChildElementBuildCache(node, &self.cache)
        })
    }
    fn next_sibling(&mut self, node: &Self::Node) -> Result<Option<Self::Node>, SourceError> {
        // SAFETY: node, cache and walker are live on this initialized thread.
        optional(unsafe {
            self.walker
                .GetNextSiblingElementBuildCache(node, &self.cache)
        })
    }
}

#[cfg(test)]
mod tests;

#[cfg(feature = "test-support")]
pub(super) mod measurement;
