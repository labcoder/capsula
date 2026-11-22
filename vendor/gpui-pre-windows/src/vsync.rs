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
    f: Box<dyn Fn() -> (bool, Instant, Instant)>,
    last_early_end: Option<Instant>,
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
            (succeeded, start, Instant::now())
        });
        Self {
            interval,
            f,
            last_early_end: None,
        }
    }

    pub(crate) fn wait_for_vsync(&mut self) -> FrameRequestSource {
        self.wait_with(std::thread::sleep)
    }

    fn wait_with(&mut self, sleep: impl FnOnce(Duration)) -> FrameRequestSource {
        let (wait_succeeded, start, end) = (self.f)();
        let early =
            wait_succeeded && end.saturating_duration_since(start) < VSYNC_INTERVAL_THRESHOLD;
        let previous_early = self
            .last_early_end
            .is_some_and(|previous| start.saturating_duration_since(previous) < self.interval * 2);
        self.last_early_end = early.then_some(end);
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
        let waits = RefCell::new((waits.to_vec().into_iter(), Instant::now()));
        VSyncProvider {
            interval: INTERVAL,
            f: Box::new(move || {
                let mut waits = waits.borrow_mut();
                let (succeeded, elapsed) = waits.0.next().expect("fixture has another wait");
                let start = waits.1;
                let end = start + elapsed;
                waits.1 = end;
                (succeeded, start, end)
            }),
            last_early_end: None,
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
    fn repeated_fast_waits_sleep_until_a_normal_wait_resets_them() {
        let mut provider = fake_provider(&[
            (true, Duration::from_nanos(999_999)),
            (true, Duration::ZERO),
            (true, Duration::from_nanos(999_999)),
            (true, Duration::from_millis(1)),
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

    #[test]
    fn an_idle_gap_expires_fast_wait_history_without_losing_backoff() {
        for (gap, elapsed, source, sleep) in [
            (
                INTERVAL * 2 - Duration::from_nanos(1),
                Duration::ZERO,
                LocalSchedule,
                Some(INTERVAL),
            ),
            (
                INTERVAL * 2 - Duration::from_nanos(1),
                VSYNC_INTERVAL_THRESHOLD - Duration::from_nanos(1),
                LocalSchedule,
                Some(INTERVAL),
            ),
            (INTERVAL * 2, Duration::ZERO, NativeCallback, None),
            (
                INTERVAL * 2 + Duration::from_nanos(1),
                Duration::ZERO,
                NativeCallback,
                None,
            ),
            (
                Duration::from_millis(200),
                Duration::ZERO,
                NativeCallback,
                None,
            ),
        ] {
            let first = Instant::now();
            let second_start = first + gap;
            let second_end = second_start + elapsed;
            let third = second_end + INTERVAL;
            let waits = RefCell::new(
                [
                    (true, first, first),
                    (true, second_start, second_end),
                    (true, third, third),
                ]
                .into_iter(),
            );
            let mut provider = VSyncProvider {
                interval: INTERVAL,
                f: Box::new(move || waits.borrow_mut().next().expect("fixture has another wait")),
                last_early_end: None,
            };
            assert_wait(&mut provider, NativeCallback, None);
            assert_wait(&mut provider, source, sleep);
            assert_wait(&mut provider, LocalSchedule, Some(INTERVAL));
        }
    }
}
