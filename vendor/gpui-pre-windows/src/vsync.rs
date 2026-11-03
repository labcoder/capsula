// Modified for Kumochi: wakes only windows that want a frame (docs/development/gpui-patch.md).
use std::{
    sync::{
        Condvar, LazyLock, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use gpui::FrameRequestSource;
use gpui_util::ResultExt;
use windows::Win32::{
    Foundation::HWND,
    Graphics::Dwm::{DWM_TIMING_INFO, DwmFlush, DwmGetCompositionTimingInfo},
    System::Performance::QueryPerformanceFrequency,
};

static QPC_TICKS_PER_SECOND: LazyLock<u64> = LazyLock::new(|| {
    let mut frequency = 0;
    // On systems that run Windows XP or later, the function will always succeed and
    // will thus never return zero.
    unsafe { QueryPerformanceFrequency(&mut frequency).unwrap() };
    frequency as u64
});

const VSYNC_INTERVAL_THRESHOLD: Duration = Duration::from_millis(1);
const DEFAULT_VSYNC_INTERVAL: Duration = Duration::from_micros(16_666); // ~60Hz

/// Wakes the vsync thread only while some window wants a frame, so a settled app sleeps.
pub(crate) static FRAME_DEMAND: FrameDemand = FrameDemand {
    pending: Mutex::new(false),
    changed: Condvar::new(),
};

pub(crate) struct FrameDemand {
    pending: Mutex<bool>,
    changed: Condvar,
}

impl FrameDemand {
    pub(crate) fn request(&self) {
        *self.pending.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.changed.notify_all();
    }

    /// Blocks until a request arrives and consumes it, or returns false once `stop` is set.
    pub(crate) fn wait(&self, stop: &AtomicBool) -> bool {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if stop.load(Ordering::Acquire) {
                return false;
            }
            if *pending {
                *pending = false;
                return true;
            }
            pending = self
                .changed
                .wait(pending)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Wakes every waiting thread without adding demand, so a stopped one exits.
    pub(crate) fn wake_all(&self) {
        // Taking the lock orders this after a waiter's stop check, so the wakeup can't be lost.
        drop(self.pending.lock().unwrap_or_else(PoisonError::into_inner));
        self.changed.notify_all();
    }
}

/// How long GPUI keeps presenting after high-rate input (its `InputRateTracker`).
const INPUT_SUSTAIN: Duration = Duration::from_secs(1);

static CLOCK_START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Milliseconds on a clock both the UI thread and the vsync thread can compare through an atomic.
pub(crate) fn clock_ms() -> u64 {
    u64::try_from(CLOCK_START.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// One window's frame demand, shared by its UI thread and the vsync thread.
#[derive(Default)]
pub(crate) struct FrameRequest {
    requested: AtomicBool,
    /// Until when, on `clock_ms`, input keeps this window's frames coming.
    input_sustain_until: AtomicU64,
}

impl FrameRequest {
    /// Asks the vsync thread to invalidate this window at the next vblank.
    pub(crate) fn request(&self) {
        self.requested.store(true, Ordering::Release);
        FRAME_DEMAND.request();
    }

    /// GPUI keeps presenting through and after high-rate input without asking for frames, so
    /// input keeps them coming for as long as GPUI might present.
    pub(crate) fn sustain_after_input(&self) {
        let until = clock_ms().saturating_add(INPUT_SUSTAIN.as_secs() * 1000);
        self.input_sustain_until.store(until, Ordering::Relaxed);
        self.request();
    }

    /// Takes a pending request.
    pub(crate) fn take(&self) -> bool {
        self.requested.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn is_sustained(&self, now_ms: u64) -> bool {
        self.input_sustain_until.load(Ordering::Relaxed) > now_ms
    }
}

pub(crate) struct VSyncProvider {
    interval: Duration,
    f: Box<dyn Fn() -> bool>,
}

impl VSyncProvider {
    pub(crate) fn new() -> Self {
        let interval = get_dwm_interval()
            .context("Failed to get DWM interval")
            .log_err()
            .unwrap_or(DEFAULT_VSYNC_INTERVAL);
        let f = Box::new(|| unsafe { DwmFlush().is_ok() });
        Self { interval, f }
    }

    pub(crate) fn wait_for_vsync(&self) -> FrameRequestSource {
        let vsync_start = Instant::now();
        let wait_succeeded = (self.f)();
        let elapsed = vsync_start.elapsed();
        // DwmFlush and DCompositionWaitForCompositorClock returns very early
        // instead of waiting until vblank when the monitor goes to sleep or is
        // unplugged (nothing to present due to desktop occlusion). We use 1ms as
        // a threshold for the duration of the wait functions and fallback to
        // Sleep() if it returns before that. This could happen during normal
        // operation for the first call after the vsync thread becomes non-idle,
        // but it shouldn't happen often.
        if !wait_succeeded || elapsed < VSYNC_INTERVAL_THRESHOLD {
            log::trace!("VSyncProvider::wait_for_vsync() took less time than expected");
            std::thread::sleep(self.interval);
            FrameRequestSource::LocalSchedule
        } else {
            FrameRequestSource::NativeCallback
        }
    }
}

fn get_dwm_interval() -> Result<Duration> {
    let mut timing_info = DWM_TIMING_INFO {
        cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32,
        ..Default::default()
    };
    unsafe { DwmGetCompositionTimingInfo(HWND::default(), &mut timing_info) }?;
    let interval = retrieve_duration(timing_info.qpcRefreshPeriod, *QPC_TICKS_PER_SECOND);
    // Check for interval values that are impossibly low. A 29 microsecond
    // interval was seen (from a qpcRefreshPeriod of 60).
    if interval < VSYNC_INTERVAL_THRESHOLD {
        Ok(retrieve_duration(
            timing_info.rateRefresh.uiDenominator as u64,
            timing_info.rateRefresh.uiNumerator as u64,
        ))
    } else {
        Ok(interval)
    }
}

#[inline]
fn retrieve_duration(counts: u64, ticks_per_second: u64) -> Duration {
    let ticks_per_microsecond = ticks_per_second / 1_000_000;
    Duration::from_micros(counts / ticks_per_microsecond)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_compositor_wait_reports_native_callback() {
        let provider = VSyncProvider {
            interval: Duration::ZERO,
            f: Box::new(|| {
                std::thread::sleep(VSYNC_INTERVAL_THRESHOLD * 2);
                true
            }),
        };

        assert_eq!(
            provider.wait_for_vsync(),
            FrameRequestSource::NativeCallback
        );
    }

    #[test]
    fn failed_compositor_wait_reports_local_schedule_even_after_threshold() {
        let provider = VSyncProvider {
            interval: Duration::ZERO,
            f: Box::new(|| {
                std::thread::sleep(VSYNC_INTERVAL_THRESHOLD * 2);
                false
            }),
        };

        assert_eq!(provider.wait_for_vsync(), FrameRequestSource::LocalSchedule);
    }
}
