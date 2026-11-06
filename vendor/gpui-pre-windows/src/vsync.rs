// Modified for Kumochi: demand-driven frames and repeated-fast-wait fallback (docs/development/gpui-patch.md).
use std::{
    sync::LazyLock,
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

mod frame_demand;

pub(crate) use frame_demand::*;

static QPC_TICKS_PER_SECOND: LazyLock<u64> = LazyLock::new(|| {
    let mut frequency = 0;
    // On systems that run Windows XP or later, the function will always succeed and
    // will thus never return zero.
    unsafe { QueryPerformanceFrequency(&mut frequency).unwrap() };
    frequency as u64
});

const VSYNC_INTERVAL_THRESHOLD: Duration = Duration::from_millis(1);
const DEFAULT_VSYNC_INTERVAL: Duration = Duration::from_micros(16_666); // ~60Hz

pub(crate) struct VSyncProvider {
    interval: Duration,
    f: Box<dyn Fn() -> (bool, Duration)>,
    early: bool,
}

impl VSyncProvider {
    pub(crate) fn new() -> Self {
        let interval = get_dwm_interval()
            .context("Failed to get DWM interval")
            .log_err()
            .unwrap_or(DEFAULT_VSYNC_INTERVAL);
        let f = Box::new(|| {
            let start = Instant::now();
            // SAFETY: DwmFlush takes no arguments and waits for the compositor.
            let succeeded = unsafe { DwmFlush().is_ok() };
            (succeeded, start.elapsed())
        });
        Self {
            interval,
            f,
            early: false,
        }
    }

    pub(crate) fn wait_for_vsync(&mut self) -> FrameRequestSource {
        self.wait_with(std::thread::sleep)
    }

    fn wait_with(&mut self, sleep: impl FnOnce(Duration)) -> FrameRequestSource {
        let (wait_succeeded, elapsed) = (self.f)();
        let early = wait_succeeded && elapsed < VSYNC_INTERVAL_THRESHOLD;
        let previous_early = std::mem::replace(&mut self.early, early);
        if !wait_succeeded || (early && previous_early) {
            log::trace!("VSyncProvider::wait_for_vsync() took less time than expected");
            sleep(self.interval);
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
    use std::cell::RefCell;

    use gpui::FrameRequestSource::{LocalSchedule, NativeCallback};

    use super::*;

    const INTERVAL: Duration = Duration::from_millis(7);

    fn fake_provider(waits: &[(bool, Duration)]) -> VSyncProvider {
        let waits = RefCell::new(waits.to_vec().into_iter());
        VSyncProvider {
            interval: INTERVAL,
            f: Box::new(move || waits.borrow_mut().next().expect("fixture has another wait")),
            early: false,
        }
    }

    fn assert_wait(
        provider: &mut VSyncProvider,
        expected_source: FrameRequestSource,
        expected_sleep: Option<Duration>,
    ) {
        let mut slept = None;
        let source = provider.wait_with(|interval| slept = Some(interval));
        assert_eq!((source, slept), (expected_source, expected_sleep));
    }

    #[test]
    fn successful_compositor_wait_reports_native_callback() {
        let mut provider = fake_provider(&[(true, VSYNC_INTERVAL_THRESHOLD * 2)]);
        assert_wait(&mut provider, NativeCallback, None);
    }

    #[test]
    fn failed_compositor_wait_reports_local_schedule_even_after_threshold() {
        let mut provider = fake_provider(&[(false, VSYNC_INTERVAL_THRESHOLD * 2)]);
        assert_wait(&mut provider, LocalSchedule, Some(INTERVAL));
    }

    #[test]
    fn first_fast_compositor_wait_does_not_sleep() {
        for elapsed in [
            Duration::ZERO,
            VSYNC_INTERVAL_THRESHOLD - Duration::from_nanos(1),
        ] {
            let mut provider = fake_provider(&[(true, elapsed)]);
            assert_wait(&mut provider, NativeCallback, None);
        }
    }

    #[test]
    fn repeated_fast_waits_sleep_until_a_normal_wait_resets_them() {
        let mut provider = fake_provider(&[
            (true, Duration::ZERO),
            (true, Duration::ZERO),
            (true, VSYNC_INTERVAL_THRESHOLD - Duration::from_nanos(1)),
            (true, VSYNC_INTERVAL_THRESHOLD),
            (true, Duration::ZERO),
            (true, Duration::ZERO),
        ]);
        assert_wait(&mut provider, NativeCallback, None);
        assert_wait(&mut provider, LocalSchedule, Some(INTERVAL));
        assert_wait(&mut provider, LocalSchedule, Some(INTERVAL));
        assert_wait(&mut provider, NativeCallback, None);
        assert_wait(&mut provider, NativeCallback, None);
        assert_wait(&mut provider, LocalSchedule, Some(INTERVAL));
    }

    #[test]
    fn failed_waits_always_sleep_and_reset_fast_successes() {
        let mut provider = fake_provider(&[
            (true, Duration::ZERO),
            (false, Duration::ZERO),
            (true, Duration::ZERO),
            (false, VSYNC_INTERVAL_THRESHOLD * 2),
            (false, Duration::ZERO),
            (true, Duration::ZERO),
        ]);
        assert_wait(&mut provider, NativeCallback, None);
        assert_wait(&mut provider, LocalSchedule, Some(INTERVAL));
        assert_wait(&mut provider, NativeCallback, None);
        assert_wait(&mut provider, LocalSchedule, Some(INTERVAL));
        assert_wait(&mut provider, LocalSchedule, Some(INTERVAL));
        assert_wait(&mut provider, NativeCallback, None);
    }
}
