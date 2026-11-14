use std::cell::Cell;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::*;
use crate::screen::Walk as _;
use crate::screen::{Availability, CaptureId, Cut, Element, ElementId, ElementTree};

#[test]
fn hresults_and_null_navigation_preserve_unavailability_without_foreign_text() {
    for (code, expected) in [
        (E_ACCESSDENIED, SourceError::Elevated),
        (
            HRESULT(UIA_E_ELEMENTNOTAVAILABLE.cast_signed()),
            SourceError::NoWindow,
        ),
        (HRESULT::from_win32(1400), SourceError::NoWindow),
        (HRESULT::from_win32(87), SourceError::NoWindow),
        (HRESULT(-7), SourceError::Fault(-7)),
    ] {
        assert_eq!(mapped(code), expected);
    }
    assert_eq!(optional::<u8>(Ok(7)), Ok(Some(7)));
    assert_eq!(optional::<u8>(Err(windows::core::Error::empty())), Ok(None));
    let result = optional::<u8>(Err(windows::core::Error::new(
        E_ACCESSDENIED,
        "native_error_sentinel",
    )));
    assert_eq!(result, Err(SourceError::Elevated));
    assert_private(&format!("{result:?}"), "native_error_sentinel");
    assert_eq!(
        io_error(std::io::Error::from_raw_os_error(5)),
        SourceError::Elevated
    );
    assert_eq!(
        io_error(std::io::Error::from_raw_os_error(87)),
        SourceError::NoWindow
    );
    assert_eq!(
        io_error(std::io::Error::other("native_error_sentinel")),
        SourceError::Fault(-1)
    );
}

#[test]
fn native_window_validation_rejects_child_hidden_missing_and_reused_windows() {
    for (root, visible, actual, expected, valid) in [
        (true, true, 17, 17, true),
        (false, true, 17, 17, false),
        (true, false, 17, 17, false),
        (true, true, 0, 0, false),
        (true, true, 18, 17, false),
    ] {
        assert_eq!(
            WindowOwner {
                root,
                visible,
                pid: actual
            }
            .matches(expected),
            valid
        );
    }
}

#[test]
fn native_rectangles_keep_negative_coordinates_and_do_not_wrap() {
    for (bounds, expected) in [
        (
            [-15, 20, -5, 30],
            PhysicalRect {
                origin: [-15, 20],
                size: [10, 10],
            },
        ),
        (
            [9, 8, 1, 2],
            PhysicalRect {
                origin: [9, 8],
                size: [0, 0],
            },
        ),
        (
            [0, 2, 0, 5],
            PhysicalRect {
                origin: [0, 2],
                size: [0, 3],
            },
        ),
        (
            [i32::MIN, i32::MIN, i32::MAX, i32::MAX],
            PhysicalRect {
                origin: [i32::MIN; 2],
                size: [u32::MAX; 2],
            },
        ),
    ] {
        let [left, top, right, bottom] = bounds;
        assert_eq!(
            rectangle(RECT {
                left,
                top,
                right,
                bottom
            }),
            expected
        );
    }
}

#[test]
fn utf16_keeps_a_scalar_of_lookahead_and_replaces_lone_surrogates() {
    for (count, kept) in [(128, 128), (129, 129), (130, 129)] {
        let units: Vec<_> = "😀".repeat(count).encode_utf16().collect();
        assert_eq!(name(&units), "😀".repeat(kept));
    }
    assert_eq!(name(&[0xd800, 0x61, 0xdc00]), "�a�");
    assert_eq!(name(&[0x61, 0, 0x62]), "a\0b");
    assert_eq!(
        name(&windows::core::BSTR::from_wide(&[0x61, 0, 0x62])),
        "a\0b"
    );
    let units: Vec<_> = format!("{}😀", "a".repeat(127)).encode_utf16().collect();
    assert_eq!(name(&units), format!("{}😀", "a".repeat(127)));
}

#[test]
fn private_cached_properties_never_retain_names() {
    let units: Vec<_> = "uia_name_sentinel".encode_utf16().collect();
    let rect = PhysicalRect {
        origin: [-10, 20],
        size: [30, 40],
    };
    for (password, offscreen, expected, calls) in [
        (false, false, "uia_name_sentinel", 1),
        (true, false, "", 0),
        (false, true, "", 0),
        (true, true, "", 0),
    ] {
        let fetched = Cell::new(0);
        let props = properties(
            NodeProps {
                name: "discard_me".into(),
                password,
                control_type: 50_001,
                rect,
            },
            if offscreen {
                Visibility::Offscreen
            } else {
                Visibility::Onscreen
            },
            || {
                fetched.set(fetched.get() + 1);
                Ok(windows::core::BSTR::from_wide(&units))
            },
        )
        .unwrap();
        assert_eq!(fetched.get(), calls);
        assert_eq!(props.name, expected);
        assert_eq!(props.password, password);
        assert_eq!(props.control_type, 50_001);
        assert_eq!(props.rect, rect);
        assert_private(&format!("{props:?}"), "uia_name_sentinel");
    }
}

#[test]
fn self_shell_and_image_exclusions_are_independent_and_lookup_failure_is_not_false() {
    for (pid, expected) in [(0, false), (11, true), (22, true), (33, false)] {
        assert_eq!(own_pid(pid, 11, 22), expected);
    }
    for (image, expected) in [
        (
            kumochi_protocol::constants::SHELL_EXE.to_ascii_uppercase(),
            true,
        ),
        (kumochi_protocol::constants::BRAIN_EXE.to_owned(), true),
        (
            format!("{}.old", kumochi_protocol::constants::SHELL_EXE),
            false,
        ),
        ("another.exe".to_owned(), false),
    ] {
        assert_eq!(image_excluded(Some(&image)), Ok(expected));
    }
    assert_eq!(image_excluded(None), Err(SourceError::Fault(-1)));
    let shared = Arc::new(AtomicU32::new(0));
    let mut native = Native(Arc::clone(&shared));
    let copy = native.clone();
    native.set_shell_pid(71);
    assert_eq!(copy.0.load(Ordering::Relaxed), 71);
    assert!(own_pid(71, 11, copy.0.load(Ordering::Relaxed)));
}

#[derive(Debug, PartialEq, Eq)]
struct Counts {
    walk: u64,
    elements: usize,
    masks: usize,
    cut: Option<Cut>,
    availability: Availability,
    elapsed_us: u128,
    text_reads: usize,
    mask_reads: usize,
    navigation_calls: usize,
    deadline_checks: usize,
    longest_check_gap_us: u128,
    tail_gap_us: u128,
}

#[derive(Default)]
struct Stats {
    text_reads: usize,
    mask_reads: usize,
    navigation_calls: usize,
    last: Option<Instant>,
    checks: usize,
    longest: Duration,
}

impl Stats {
    fn observe(&mut self, now: Instant) -> Instant {
        if let Some(last) = self.last.replace(now) {
            self.longest = self.longest.max(now.saturating_duration_since(last));
        }
        self.checks += 1;
        now
    }
}

struct Metered<S> {
    source: S,
    stats: Arc<Mutex<Stats>>,
}

impl<S: ElementSource> ElementSource for Metered<S> {
    type Node = S::Node;
    fn root(
        &mut self,
        target: ElementTarget,
    ) -> Result<(Self::Node, PhysicalRect, u32), SourceError> {
        self.source.root(target)
    }
    fn read(&mut self, node: &Self::Node) -> Result<NodeProps, SourceError> {
        self.stats.lock().unwrap().text_reads += 1;
        self.source.read(node)
    }
    fn mask(&mut self, node: &Self::Node) -> Result<(bool, PhysicalRect), SourceError> {
        self.stats.lock().unwrap().mask_reads += 1;
        self.source.mask(node)
    }
    fn first_child(&mut self, node: &Self::Node) -> Result<Option<Self::Node>, SourceError> {
        self.stats.lock().unwrap().navigation_calls += 1;
        self.source.first_child(node)
    }
    fn next_sibling(&mut self, node: &Self::Node) -> Result<Option<Self::Node>, SourceError> {
        self.stats.lock().unwrap().navigation_calls += 1;
        self.source.next_sibling(node)
    }
}

fn counts(
    walk: u64,
    tree: &ElementTree,
    masks: usize,
    elapsed: Duration,
    stats: &Stats,
    end: Instant,
) -> Counts {
    Counts {
        walk,
        elements: tree.elements.len(),
        masks,
        cut: tree.cut,
        availability: tree.availability,
        elapsed_us: elapsed.as_micros(),
        text_reads: stats.text_reads,
        mask_reads: stats.mask_reads,
        navigation_calls: stats.navigation_calls,
        deadline_checks: stats.checks,
        longest_check_gap_us: stats.longest.as_micros(),
        tail_gap_us: stats
            .last
            .map_or(0, |last| end.saturating_duration_since(last).as_micros()),
    }
}

fn assert_private(text: &str, sentinel: &str) {
    assert!(!text.contains(sentinel));
    assert!(!text.contains(&format!("{:?}", sentinel.as_bytes())));
}

#[test]
fn smoke_output_is_structurally_counts_and_timings_not_window_content() {
    let rect = PhysicalRect {
        origin: [12345, 67890],
        size: [1, 1],
    };
    let tree = ElementTree {
        capture: CaptureId(1),
        window: rect,
        cut: Some(Cut::Budget),
        availability: Availability::Read,
        elements: vec![Element {
            id: ElementId {
                capture: CaptureId(1),
                index: 0,
            },
            parent: None,
            name: "window_title_and_name_sentinel".into(),
            name_cut: false,
            password: false,
            control_type: 50_000,
            screen: rect,
            in_window: rect,
        }],
    };
    let start = Instant::now();
    let mut stats = Stats {
        text_reads: 1,
        mask_reads: 3,
        navigation_calls: 6,
        ..Stats::default()
    };
    for offset in [0, 7_000, 11_000] {
        stats.observe(start + Duration::from_micros(offset));
    }
    let summary = counts(
        2,
        &tree,
        2,
        Duration::from_micros(13_800),
        &stats,
        start + Duration::from_micros(13_800),
    );
    assert_eq!(
        summary,
        Counts {
            walk: 2,
            elements: 1,
            masks: 2,
            cut: Some(Cut::Budget),
            availability: Availability::Read,
            elapsed_us: 13_800,
            text_reads: 1,
            mask_reads: 3,
            navigation_calls: 6,
            deadline_checks: 3,
            longest_check_gap_us: 7_000,
            tail_gap_us: 2_800,
        }
    );
    let printed = format!("{summary:?}");
    assert_private(&printed, "window_title_and_name_sentinel");
    assert!(!printed.contains("12345"));
    assert!(!printed.contains("67890"));
}

fn selected_window_walks(top: &TopWindow) -> Availability {
    let stats = Arc::new(Mutex::new(Stats::default()));
    let source_stats = Arc::clone(&stats);
    let clock_stats = Arc::clone(&stats);
    let shell = Arc::new(AtomicU32::new(0));
    let source_shell = Arc::clone(&shell);
    let mut reader = ThreadedReader::with_clock(
        move || {
            Automation::new(Arc::clone(&source_shell))
                .map(|source| Metered {
                    source,
                    stats: Arc::clone(&source_stats),
                })
                .map_err(|error| error.code().0)
        },
        Native(shell),
        move || clock_stats.lock().unwrap().observe(Instant::now()),
        || true,
    )
    .expect("reader thread opens");
    let mut availability = Availability::Unavailable(crate::screen::Unavailable::Busy);
    for capture in 1..=2 {
        *stats.lock().unwrap() = Stats::default();
        let masks = Arc::new(Mutex::new(crate::screen::MaskSet::new(CaptureId(capture))));
        let start = Instant::now();
        let walked = reader
            .walk(crate::screen::WalkJob {
                capture: CaptureId(capture),
                target: ElementTarget::Window {
                    window: top.window,
                    pid: top.pid,
                },
                display: top.rect,
                until: start + Duration::from_millis(120),
                masks: Arc::clone(&masks),
            })
            .expect("the walk returns a numeric result");
        let end = Instant::now();
        availability = walked.tree.availability;
        println!(
            "{:?}",
            counts(
                capture,
                &walked.tree,
                masks.lock().unwrap().rects().len(),
                end.saturating_duration_since(start),
                &stats.lock().unwrap(),
                end
            )
        );
        if walked.retire || availability != Availability::Read {
            break;
        }
    }
    availability
}

fn explicit_test_window() -> TopWindow {
    let window = std::env::var("UIA_TEST_HWND")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .expect("UIA_TEST_HWND must name an explicit owned fixture");
    let pid = std::env::var("UIA_TEST_PID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .expect("UIA_TEST_PID must name the explicit fixture owner");
    let handle = HWND(std::ptr::without_provenance_mut(window));
    snapshot(handle, Some(pid)).expect("the explicit fixture must be a visible owned root")
}

#[test]
#[ignore = "explicit owned window in UIA_TEST_HWND and UIA_TEST_PID; never foreground"]
fn real_window_walk_smoke() {
    let top = explicit_test_window();
    assert_eq!(selected_window_walks(&top), Availability::Read);
}

#[test]
#[ignore = "explicit newly launched Settings window; the external wrapper owns launch and cleanup"]
fn real_uwp_window_owner_pid_smoke() {
    let top = explicit_test_window();
    let shell = Arc::new(AtomicU32::new(0));
    let mut source = Automation::new(shell)
        .map_err(|error| error.code().0)
        .expect("UIA opens");
    let (_, _, owner) = source
        .root(ElementTarget::Window {
            window: top.window,
            pid: top.pid,
        })
        .unwrap();
    assert!(owner == top.pid, "native owner differs from the pin");
    assert_eq!(selected_window_walks(&top), Availability::Read);
}

#[test]
fn timing_instrumentation_counts_navigation_and_mask_reads_separately() {
    struct One(PhysicalRect);
    impl ElementSource for One {
        type Node = ();
        fn root(&mut self, _: ElementTarget) -> Result<((), PhysicalRect, u32), SourceError> {
            Ok(((), self.0, 8))
        }
        fn read(&mut self, (): &()) -> Result<NodeProps, SourceError> {
            Ok(NodeProps {
                name: String::new(),
                password: true,
                control_type: 0,
                rect: self.0,
            })
        }
        fn first_child(&mut self, (): &()) -> Result<Option<()>, SourceError> {
            Ok(None)
        }
        fn next_sibling(&mut self, (): &()) -> Result<Option<()>, SourceError> {
            Ok(None)
        }
    }
    let rect = PhysicalRect {
        origin: [0; 2],
        size: [1; 2],
    };
    let stats = Arc::new(Mutex::new(Stats::default()));
    let mut source = Metered {
        source: One(rect),
        stats: Arc::clone(&stats),
    };
    assert_eq!(source.root(ElementTarget::Foreground), Ok(((), rect, 8)));
    assert!(source.read(&()).unwrap().password);
    assert_eq!(source.mask(&()), Ok((true, rect)));
    assert_eq!(source.first_child(&()), Ok(None));
    assert_eq!(source.next_sibling(&()), Ok(None));
    let stats = stats.lock().unwrap();
    assert_eq!(stats.text_reads, 1);
    assert_eq!(stats.mask_reads, 1);
    assert_eq!(stats.navigation_calls, 2);
}

#[test]
fn partial_com_initialization_balances_failure_unwind_and_changed_mode() {
    use windows::Win32::System::Com::{
        APTTYPE, APTTYPEQUALIFIER, COINIT_APARTMENTTHREADED, CoGetApartmentType,
    };
    fn apartment_status() -> Result<(), HRESULT> {
        let mut kind = APTTYPE::default();
        let mut qualifier = APTTYPEQUALIFIER::default();
        // SAFETY: both output arguments are valid writable values on the calling thread.
        unsafe { CoGetApartmentType(&raw mut kind, &raw mut qualifier) }
            .map_err(|error| error.code())
    }
    std::thread::spawn(|| {
        let uninitialized = Err(HRESULT(0x8004_01f0_u32.cast_signed()));
        assert_eq!(apartment_status(), uninitialized);
        let failed = (|| -> Result<(), HRESULT> {
            let _apartment = Apartment::enter().map_err(|error| error.code())?;
            assert_eq!(apartment_status(), Ok(()));
            Err(HRESULT(-7))
        })();
        assert_eq!(failed, Err(HRESULT(-7)));
        assert_eq!(apartment_status(), uninitialized);
        assert!(
            std::panic::catch_unwind(|| {
                let _apartment = Apartment::enter().unwrap();
                panic!("partial setup fixture");
            })
            .is_err()
        );
        assert_eq!(apartment_status(), uninitialized);
        // SAFETY: this fresh test thread has no apartment and balances success with the guard.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .unwrap();
        let sta = Apartment(PhantomData);
        assert_eq!(
            Apartment::enter().err().unwrap().code(),
            HRESULT(0x8001_0106_u32.cast_signed())
        );
        assert_eq!(apartment_status(), Ok(()));
        drop(sta);
        assert_eq!(apartment_status(), uninitialized);
    })
    .join()
    .unwrap();
}
