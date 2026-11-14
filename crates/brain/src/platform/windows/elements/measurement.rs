//! Timing access to the UIA reader for one explicit window, compiled only with `test-support`.
//!
//! It measures what a screen walk costs on a real window: retained elements and masks, the
//! walk plus serialization time, and, through a [`WindowProbe`], provider navigation calls and
//! the walk's deadline checks. An outside harness drives it against a window that harness owns,
//! named by handle and owner pid; it never targets the foreground window. Nothing here ships.

use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, TrySendError};
use std::time::{Duration, Instant};

use super::{
    Arc, AtomicU32, Automation, ElementReader, ElementSource, ElementTarget, HWND,
    IUIAutomationElement, Native, NodeProps, PhysicalRect, SourceError, ThreadedReader, TopWindow,
    snapshot,
};
use crate::screen::{
    Availability, CONNECTION_TIMEOUT, CaptureId, Failure, MaskSet, TRANSACTION_TIMEOUT, WalkJob,
};

/// Observes provider navigation and the walk's existing deadline checks.
pub trait WindowProbe: Send + Sync + 'static {
    /// Returns the monotonic time at an existing deadline check.
    fn now(&self) -> Instant;
    /// Records one provider navigation call.
    fn navigation(&self);
}

struct Observed {
    source: Automation,
    probe: Arc<dyn WindowProbe>,
}

impl ElementSource for Observed {
    type Node = IUIAutomationElement;
    fn root(
        &mut self,
        target: ElementTarget,
    ) -> Result<(Self::Node, PhysicalRect, u32), SourceError> {
        self.source.root(target)
    }
    fn read(&mut self, node: &Self::Node) -> Result<NodeProps, SourceError> {
        self.source.read(node)
    }
    fn mask(&mut self, node: &Self::Node) -> Result<(bool, PhysicalRect), SourceError> {
        self.source.mask(node)
    }
    fn first_child(&mut self, node: &Self::Node) -> Result<Option<Self::Node>, SourceError> {
        self.probe.navigation();
        self.source.first_child(node)
    }
    fn next_sibling(&mut self, node: &Self::Node) -> Result<Option<Self::Node>, SourceError> {
        self.probe.navigation();
        self.source.next_sibling(node)
    }
}

/// Structural results only; the text reply is serialized in memory and discarded.
#[derive(Debug)]
pub struct WindowSample {
    /// Number of retained elements.
    pub elements: usize,
    /// Whether the explicit target was readable.
    pub availability: Availability,
    /// Number of retained redaction masks.
    pub masks: usize,
    /// Walk and serialization duration in microseconds.
    pub elapsed_us: u128,
    /// Start of the walk, including lazy UIA setup before the first check.
    pub started: Instant,
    /// End of serialization, for the unchecked-tail calculation.
    pub ended: Instant,
    /// Native window DPI.
    pub dpi: u32,
}

/// One bounded reader pinned to an explicit native window, never the foreground.
pub struct WindowSession {
    reader: ThreadedReader<Native>,
    wakes: Receiver<()>,
    top: TopWindow,
    dpi: u32,
    capture: u64,
}

impl std::fmt::Debug for WindowSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowSession")
            .field("dpi", &self.dpi)
            .field("samples", &self.capture)
            .finish_non_exhaustive()
    }
}

impl WindowSession {
    /// Opens the thread; UIA initialization remains inside the first timed walk.
    ///
    /// # Errors
    /// Returns a numeric failure if the explicit window or thread cannot be opened.
    pub fn open(window: u64, pid: u32, probe: Arc<dyn WindowProbe>) -> Result<Self, i32> {
        let handle = HWND(std::ptr::without_provenance_mut(
            usize::try_from(window).map_err(|_| -1)?,
        ));
        let top = snapshot(handle, Some(pid)).map_err(|_| -1)?;
        // SAFETY: the handle has been validated as a visible root owned by the explicit pid.
        let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(handle) };
        if dpi == 0 {
            return Err(-1);
        }
        let shell = Arc::new(AtomicU32::new(0));
        let source_shell = Arc::clone(&shell);
        let source_probe = Arc::clone(&probe);
        let (wake, wakes) = mpsc::sync_channel(1);
        let reader = ThreadedReader::with_clock(
            move || {
                Automation::new(Arc::clone(&source_shell))
                    .map(|source| Observed {
                        source,
                        probe: Arc::clone(&source_probe),
                    })
                    .map_err(|error| error.code().0)
            },
            Native(shell),
            move || probe.now(),
            move || !matches!(wake.try_send(()), Err(TrySendError::Disconnected(()))),
        )?;
        Ok(Self {
            reader,
            wakes,
            top,
            dpi,
            capture: 0,
        })
    }

    /// Includes the bounded walk and complete-tree serialization, not process startup.
    ///
    /// # Errors
    /// Returns a numeric failure if reading or serializing the explicit window fails.
    pub fn sample(&mut self) -> Result<WindowSample, i32> {
        self.capture = self.capture.checked_add(1).ok_or(-1)?;
        let start = Instant::now();
        let masks = Arc::new(Mutex::new(MaskSet::new(CaptureId(self.capture))));
        let until = start + Duration::from_millis(120);
        let job = WalkJob {
            target: ElementTarget::Window {
                window: self.top.window,
                pid: self.top.pid,
            },
            capture: CaptureId(self.capture),
            display: self.top.rect,
            until,
            masks: Arc::clone(&masks),
        };
        let walked = if let Err(walked) = self.reader.begin(job) {
            walked
        } else {
            let slack = until + CONNECTION_TIMEOUT + TRANSACTION_TIMEOUT;
            self.wakes
                .recv_timeout(slack.saturating_duration_since(Instant::now()))
                .map_err(|_| -1)?;
            self.reader.poll().ok_or(-1)?.map_err(|error| match error {
                Failure::Fault(code) | Failure::Retire(code) => code,
                Failure::Refused(_) => -1,
            })?
        };
        drop(serde_json::to_vec(&walked.tree).map_err(|_| -1)?);
        let ended = Instant::now();
        let masks = masks.lock().map_err(|_| -1)?.rects().len();
        Ok(WindowSample {
            elements: walked.tree.elements.len(),
            availability: walked.tree.availability,
            masks,
            elapsed_us: ended.saturating_duration_since(start).as_micros(),
            started: start,
            ended,
            dpi: self.dpi,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE,
    };
    use windows::core::{PCWSTR, w};

    use super::{WindowProbe, WindowSession};

    struct Unobserved;
    impl WindowProbe for Unobserved {
        fn now(&self) -> Instant {
            Instant::now()
        }
        fn navigation(&self) {}
    }

    #[test]
    fn open_refuses_a_bad_handle_and_a_window_another_pid_owns() {
        let pid = std::process::id();
        assert_eq!(
            WindowSession::open(0, pid, Arc::new(Unobserved)).unwrap_err(),
            -1
        );
        // SAFETY: creates a 1x1 off-screen, non-activating popup owned by this thread.
        let window = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                PCWSTR::null(),
                WS_POPUP | WS_VISIBLE,
                -32_000,
                -32_000,
                1,
                1,
                None,
                None,
                None,
                None,
            )
        }
        .expect("a test window");
        let handle = u64::try_from(window.0.addr()).expect("a handle fits in u64");
        let wrong = WindowSession::open(handle, pid.wrapping_add(4), Arc::new(Unobserved));
        let right = WindowSession::open(handle, pid, Arc::new(Unobserved));
        // SAFETY: the window was created above on this thread and is destroyed once.
        unsafe { DestroyWindow(window) }.expect("the test window closes");
        assert_eq!(wrong.unwrap_err(), -1);
        right.expect("the owner's own pid opens the same window");
    }
}
