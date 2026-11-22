use super::*;
use crate::screen::frame::RawFrame;
use crate::screen::mask::{MaskSet, Unavailable};
use crate::screen::sanitize::sanitize;
use crate::screen::wire::FrameMeta;

fn rect(origin: [i32; 2], size: [u32; 2]) -> PhysicalRect {
    PhysicalRect { origin, size }
}
fn frame(display: PhysicalRect) -> RawFrame {
    let mut raw = RawFrame::default();
    for pixel in raw
        .begin(display.size[0], display.size[1])
        .unwrap()
        .as_chunks_mut::<4>()
        .0
    {
        pixel.copy_from_slice(&[23, 31, 47, 255]);
    }
    raw.commit(FrameMeta {
        capture: CaptureId(1),
        display,
        dpi: 96,
        acquired_after_ms: 0,
    })
    .unwrap();
    raw
}
fn masks(completeness: Completeness) -> MaskSink {
    let mut set = MaskSet::new(CaptureId(1));
    if completeness != Completeness::Pending {
        set.finish(completeness);
    }
    Arc::new(std::sync::Mutex::new(set))
}
fn tree(window: PhysicalRect) -> ElementTree {
    ElementTree {
        capture: CaptureId(1),
        window,
        elements: Vec::new(),
        cut: None,
        availability: Availability::Read,
    }
}
fn word(text: &str, bounds: [f64; 4]) -> NativeWord {
    NativeWord {
        text: text.into(),
        bounds,
    }
}
fn until() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

struct Fake {
    words: Vec<NativeWord>,
    max: u32,
}
impl Engine for Fake {
    fn max_dimension(&self) -> u32 {
        self.max
    }
    fn recognize(
        &mut self,
        _: &Bitmap,
        stop: &Stop,
        emit: &mut dyn FnMut(NativeWord) -> bool,
    ) -> Result<(), Failure> {
        for word in self.words.drain(..) {
            stop.check()?;
            if !emit(word) {
                break;
            }
        }
        Ok(())
    }
}
fn run(words: Vec<NativeWord>, set: &MaskSink, region: PhysicalRect, max: u32) -> OcrWords {
    let mut raw = frame(rect([-10, -20], [8, 4]));
    let clean = sanitize(&mut raw, &set.lock().unwrap()).unwrap();
    let region = OcrRegion::window(&clean, &tree(region)).unwrap();
    let mut recognizer = TextRecognizer::start(move || Ok(Fake { words, max })).unwrap();
    let (_end, ended) = mpsc::channel();
    recognizer
        .recognize(&clean, region, set, &ended, until())
        .unwrap()
}

#[test]
#[cfg(windows)]
#[ignore = "manual native OCR smoke; synthetic blank pixels only, never in CI"]
fn native_blank_bgra_bitmap_has_no_words() {
    let mut raw = frame(rect([0, 0], [64, 64]));
    let set = masks(Completeness::Complete);
    let clean = sanitize(&mut raw, &set.lock().unwrap()).unwrap();
    let region = OcrRegion::window(&clean, &tree(clean.image().0.display)).unwrap();
    let mut recognizer = TextRecognizer::new().expect("native English OCR opens");
    let (_end, ended) = mpsc::channel();
    let output = recognizer
        .recognize(&clean, region, &set, &ended, until())
        .expect("native English OCR accepts a blank BGRA bitmap");
    assert_eq!(output.words.len(), 0);
    assert!(!output.truncated);
}

#[test]
fn trigger_uses_threshold_and_exact_focused_native_class() {
    assert!(should_recognize(19, "Other"));
    assert!(!should_recognize(20, "Other"));
    assert!(should_recognize(20, "Chrome_RenderWidgetHostHWND"));
    assert!(!should_recognize(20, "chrome_renderwidgethosthwnd"));
    assert!(!should_recognize(20, "Chrome_WidgetWin_1"));
}

#[test]
fn typed_regions_clip_focus_and_walk_but_allow_maximized_windows() {
    let display = rect([-10, -20], [8, 4]);
    let mut raw = frame(display);
    let clean = sanitize(&mut raw, &masks(Completeness::Complete).lock().unwrap()).unwrap();
    assert_eq!(
        OcrRegion::window(&clean, &tree(display)).unwrap().rect,
        display
    );
    let transform = CaptureTransform {
        display,
        dpi: 96,
        overview_size: [8, 4],
        crop: Some(rect([-12, -19], [5, 2])),
    };
    assert_eq!(
        OcrRegion::focus(&clean, transform).unwrap().rect,
        rect([-10, -19], [3, 2])
    );
    assert_eq!(
        OcrRegion::focus(
            &clean,
            CaptureTransform {
                crop: None,
                ..transform
            }
        )
        .unwrap_err(),
        Failure::Invalid
    );
    assert_eq!(
        OcrRegion::focus(
            &clean,
            CaptureTransform {
                display: rect([0, 0], [8, 4]),
                ..transform
            }
        )
        .unwrap_err(),
        Failure::Invalid
    );
    for invalid in [rect([-10, -20], [0, 4]), rect([0, 0], [1, 1])] {
        assert_eq!(
            OcrRegion::window(&clean, &tree(invalid)).unwrap_err(),
            Failure::Invalid
        );
    }
    let mut stale = tree(display);
    stale.capture = CaptureId(2);
    assert_eq!(
        OcrRegion::window(&clean, &stale).unwrap_err(),
        Failure::Invalid
    );
    stale.capture = CaptureId(1);
    stale.availability = Availability::Unavailable(Unavailable::Busy);
    assert_eq!(
        OcrRegion::window(&clean, &stale).unwrap_err(),
        Failure::Invalid
    );
}

#[test]
fn output_translates_origin_clips_and_rounds_outward() {
    let set = masks(Completeness::Complete);
    let output = run(
        vec![
            word("a", [0.2, 0.1, 1.1, 0.2]),
            word("b", [-2.0, 0.0, 3.0, 9.0]),
            word("off", [9.0, 0.0, 1.0, 1.0]),
            word("nan", [f64::NAN, 0.0, 1.0, 1.0]),
            word("empty", [0.0, 0.0, 0.0, 1.0]),
        ],
        &set,
        rect([-8, -19], [5, 2]),
        100,
    );
    assert_eq!(
        output
            .words
            .iter()
            .map(|word| (word.text.as_str(), word.rect))
            .collect::<Vec<_>>(),
        [
            ("a", rect([-8, -19], [2, 1])),
            ("b", rect([-8, -19], [1, 2]))
        ]
    );
    assert!(!output.truncated);
}

#[test]
fn caps_are_exact_and_utf8_safe_after_mask_filtering() {
    let set = masks(Completeness::Complete);
    let region = rect([-10, -20], [8, 4]);
    for (count, truncated) in [(0, false), (2_000, false), (2_001, true)] {
        let output = run(
            (0..count)
                .map(|_| word("x", [0.0, 0.0, 1.0, 1.0]))
                .collect(),
            &set,
            region,
            100,
        );
        assert_eq!(output.words.len(), count.min(2_000));
        assert_eq!(output.truncated, truncated);
    }
    for (text, expected, truncated) in [
        ("x".repeat(32_768), 32_768, false),
        ("x".repeat(32_769), 32_768, true),
        (format!("{}é", "x".repeat(32_767)), 32_767, true),
    ] {
        let output = run(vec![word(&text, [0.0, 0.0, 1.0, 1.0])], &set, region, 100);
        assert_eq!(output.words[0].text.len(), expected);
        assert_eq!(output.truncated, truncated);
    }
    for (lengths, kept, truncated) in [
        (vec![16_384, 16_384], vec![16_384, 16_384], false),
        (vec![16_384, 16_384, 1], vec![16_384, 16_384], true),
        (vec![16_384, 16_385], vec![16_384, 16_384], true),
    ] {
        let output = run(
            lengths
                .into_iter()
                .map(|len| word(&"x".repeat(len), [0.0, 0.0, 1.0, 1.0]))
                .collect(),
            &set,
            region,
            100,
        );
        assert_eq!(
            output
                .words
                .iter()
                .map(|word| word.text.len())
                .collect::<Vec<_>>(),
            kept
        );
        assert_eq!(output.truncated, truncated);
    }
    set.lock().unwrap().push(rect([-10, -20], [1, 1]));
    let mut words = (0..2_001)
        .map(|_| word("hidden", [0.1, 0.1, 1.0, 1.0]))
        .collect::<Vec<_>>();
    words.push(word(&"x".repeat(32_769), [0.1, 0.1, 1.0, 1.0]));
    words.push(word("wholly inside", [0.0, 0.0, 0.5, 0.5]));
    words.push(word("visible", [1.0, 0.0, 1.0, 1.0]));
    let output = run(words, &set, region, 100);
    assert_eq!(output.words.len(), 1);
    assert_eq!(output.words[0].text, "visible");
    assert!(!output.truncated);
}

#[test]
fn sanitized_region_is_only_source_and_masks_are_exactly_clipped() {
    struct Inspect;
    impl Engine for Inspect {
        fn max_dimension(&self) -> u32 {
            100
        }
        #[expect(
            clippy::panic_in_result_fn,
            reason = "fake asserts synthetic pixel inputs"
        )]
        fn recognize(
            &mut self,
            image: &Bitmap,
            _: &Stop,
            _: &mut dyn FnMut(NativeWord) -> bool,
        ) -> Result<(), Failure> {
            assert_eq!(image.bgra.dimensions(), (3, 2));
            assert_eq!(image.bgra.get_pixel(0, 0).0, [0, 0, 0, 255]);
            assert_eq!(image.bgra.get_pixel(1, 0).0, [23, 31, 47, 255]);
            Ok(())
        }
    }
    let set = masks(Completeness::Complete);
    set.lock().unwrap().push(rect([-12, -22], [3, 3]));
    set.lock().unwrap().push(rect([99, 99], [3, 3]));
    let mut raw = frame(rect([-10, -20], [8, 4]));
    let clean = sanitize(&mut raw, &set.lock().unwrap()).unwrap();
    assert_eq!(clean.painted(), &[rect([-10, -20], [1, 1])]);
    let region = OcrRegion::window(&clean, &tree(rect([-10, -20], [3, 2]))).unwrap();
    let mut recognizer = TextRecognizer::start(|| Ok(Inspect)).unwrap();
    let (_end, ended) = mpsc::channel();
    assert_eq!(
        recognizer
            .recognize(&clean, region, &set, &ended, until())
            .unwrap()
            .words
            .len(),
        0
    );
}

#[test]
fn one_pixel_over_maximum_scales_aspect_and_inverse_bounds_before_filtering() {
    struct Scaled;
    impl Engine for Scaled {
        fn max_dimension(&self) -> u32 {
            3
        }
        #[expect(
            clippy::panic_in_result_fn,
            reason = "fake asserts synthetic scaling inputs"
        )]
        fn recognize(
            &mut self,
            image: &Bitmap,
            _: &Stop,
            emit: &mut dyn FnMut(NativeWord) -> bool,
        ) -> Result<(), Failure> {
            assert_eq!(image.bgra.dimensions(), (3, 1));
            assert!(emit(word("masked", [0.1, 0.0, 0.8, 0.4])));
            assert!(emit(word("kept", [1.6, 0.1, 0.5, 0.4])));
            Ok(())
        }
    }
    let set = masks(Completeness::Partial);
    set.lock().unwrap().push(rect([-10, -20], [1, 1]));
    let mut raw = frame(rect([-10, -20], [8, 4]));
    let clean = sanitize(&mut raw, &set.lock().unwrap()).unwrap();
    let region = OcrRegion::window(&clean, &tree(rect([-10, -20], [4, 2]))).unwrap();
    let mut recognizer = TextRecognizer::start(|| Ok(Scaled)).unwrap();
    let (_end, ended) = mpsc::channel();
    let output = recognizer
        .recognize(&clean, region, &set, &ended, until())
        .unwrap();
    assert_eq!(output.words.len(), 1);
    assert_eq!(output.words[0].text, "kept");
    assert_eq!(output.words[0].rect, rect([-8, -20], [1, 1]));
}

#[test]
fn pending_is_refused_and_completed_completeness_is_preserved() {
    let set = masks(Completeness::Pending);
    let mut raw = frame(rect([-10, -20], [8, 4]));
    let clean = sanitize(&mut raw, &set.lock().unwrap()).unwrap();
    let region = OcrRegion::window(&clean, &tree(clean.image().0.display)).unwrap();
    let mut recognizer = TextRecognizer::start(|| {
        Ok(Fake {
            words: Vec::new(),
            max: 100,
        })
    })
    .unwrap();
    let (_end, ended) = mpsc::channel();
    assert_eq!(
        recognizer
            .recognize(&clean, region, &set, &ended, until())
            .unwrap_err(),
        Failure::Pending
    );
    for completeness in [
        Completeness::Complete,
        Completeness::Partial,
        Completeness::Unknown(Unavailable::Busy),
    ] {
        let output = run(
            vec![word("kept", [0.0, 0.0, 1.0, 1.0])],
            &masks(completeness),
            clean.image().0.display,
            100,
        );
        assert_eq!(output.capture, CaptureId(1));
        assert_eq!(output.generation, 1);
        assert_eq!(output.completeness, completeness);
        assert_eq!(output.words.len(), 1);
        assert_eq!(output.words[0].text, "kept");
    }
}

struct Blocking {
    entered: mpsc::SyncSender<()>,
    release: Arc<AtomicBool>,
}
impl Engine for Blocking {
    fn max_dimension(&self) -> u32 {
        100
    }
    fn recognize(
        &mut self,
        _: &Bitmap,
        stop: &Stop,
        emit: &mut dyn FnMut(NativeWord) -> bool,
    ) -> Result<(), Failure> {
        self.entered.send(()).unwrap();
        loop {
            stop.check()?;
            if self.release.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(POLL);
        }
        emit(word("private marker", [0.0, 0.0, 1.0, 1.0]));
        Ok(())
    }
}

#[test]
fn newly_published_mask_or_capture_supersedes_blocked_completion() {
    for replace_capture in [false, true] {
        let (entered, started) = mpsc::sync_channel(1);
        let release = Arc::new(AtomicBool::new(false));
        let blocked = Arc::clone(&release);
        let mut recognizer = TextRecognizer::start(move || {
            Ok(Blocking {
                entered,
                release: blocked,
            })
        })
        .unwrap();
        let set = masks(Completeness::Complete);
        let mut raw = frame(rect([-10, -20], [8, 4]));
        let clean = sanitize(&mut raw, &set.lock().unwrap()).unwrap();
        let region = OcrRegion::window(&clean, &tree(clean.image().0.display)).unwrap();
        let (_end, ended) = mpsc::channel();
        std::thread::scope(|scope| {
            let recognizer = &mut recognizer;
            let clean = &clean;
            let job_masks = Arc::clone(&set);
            let result = scope
                .spawn(move || recognizer.recognize(clean, region, &job_masks, &ended, until()));
            started.recv_timeout(Duration::from_secs(2)).unwrap();
            if replace_capture {
                *set.lock().unwrap() = MaskSet::new(CaptureId(2));
            } else {
                set.lock().unwrap().push(rect([-10, -20], [1, 1]));
            }
            release.store(true, Ordering::Release);
            assert_eq!(result.join().unwrap().unwrap_err(), Failure::Superseded);
        });
    }
}

#[test]
fn missing_english_is_fixed_unavailable_even_on_later_attempts() {
    let mut recognizer =
        TextRecognizer::start(|| -> Result<Fake, Failure> { Err(Failure::Unavailable) }).unwrap();
    let set = masks(Completeness::Complete);
    let mut raw = frame(rect([-10, -20], [8, 4]));
    let clean = sanitize(&mut raw, &set.lock().unwrap()).unwrap();
    let region = OcrRegion::window(&clean, &tree(clean.image().0.display)).unwrap();
    let (_end, ended) = mpsc::channel();
    for _ in 0..2 {
        let error = recognizer
            .recognize(&clean, region, &set, &ended, until())
            .unwrap_err();
        assert_eq!(error, Failure::Unavailable);
        assert_eq!(error.to_string(), "OCR unavailable");
    }
}

#[test]
fn response_wait_sees_cancel_eof_deadline_and_cannot_revive() {
    for failure in [Failure::Cancelled, Failure::Closed, Failure::Deadline] {
        let (entered, started) = mpsc::sync_channel(1);
        let release = Arc::new(AtomicBool::new(false));
        let blocked = Arc::clone(&release);
        let mut recognizer = TextRecognizer::start(move || {
            Ok(Blocking {
                entered,
                release: blocked,
            })
        })
        .unwrap();
        let set = masks(Completeness::Complete);
        let mut raw = frame(rect([-10, -20], [8, 4]));
        let clean = sanitize(&mut raw, &set.lock().unwrap()).unwrap();
        let region = OcrRegion::window(&clean, &tree(clean.image().0.display)).unwrap();
        let (end, ended) = mpsc::channel();
        let deadline = if failure == Failure::Deadline {
            Instant::now() + Duration::from_secs(1)
        } else {
            until()
        };
        std::thread::scope(|scope| {
            let recognizer = &mut recognizer;
            let clean = &clean;
            let job_masks = Arc::clone(&set);
            let result = scope
                .spawn(move || recognizer.recognize(clean, region, &job_masks, &ended, deadline));
            started.recv_timeout(Duration::from_secs(2)).unwrap();
            if failure == Failure::Cancelled {
                end.send(()).unwrap();
            }
            if failure == Failure::Closed {
                drop(end);
            }
            if failure == Failure::Deadline {
                std::thread::sleep(
                    deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(10),
                );
                release.store(true, Ordering::Release);
            }
            assert_eq!(result.join().unwrap().unwrap_err(), failure);
        });
        let (_fresh, ended) = mpsc::channel();
        assert_eq!(
            recognizer
                .recognize(&clean, region, &set, &ended, until())
                .unwrap_err(),
            Failure::Closed
        );
    }
}

#[test]
fn native_failure_stays_failed_on_later_valid_attempt() {
    struct FailsOnce(bool);
    impl Engine for FailsOnce {
        fn max_dimension(&self) -> u32 {
            100
        }
        fn recognize(
            &mut self,
            _: &Bitmap,
            _: &Stop,
            emit: &mut dyn FnMut(NativeWord) -> bool,
        ) -> Result<(), Failure> {
            if !self.0 {
                self.0 = true;
                return Err(Failure::Unavailable);
            }
            emit(word("must not revive", [0.0, 0.0, 1.0, 1.0]));
            Ok(())
        }
    }
    let set = masks(Completeness::Complete);
    let mut raw = frame(rect([-10, -20], [8, 4]));
    let clean = sanitize(&mut raw, &set.lock().unwrap()).unwrap();
    let region = OcrRegion::window(&clean, &tree(clean.image().0.display)).unwrap();
    let mut recognizer = TextRecognizer::start(|| Ok(FailsOnce(false))).unwrap();
    let (_end, ended) = mpsc::channel();
    for _ in 0..2 {
        assert_eq!(
            recognizer
                .recognize(&clean, region, &set, &ended, until())
                .unwrap_err(),
            Failure::Unavailable
        );
    }
}

#[test]
fn admission_and_mask_lock_waits_see_cancel_eof_and_deadline() {
    for at_mask_lock in [false, true] {
        for failure in [Failure::Cancelled, Failure::Closed, Failure::Deadline] {
            let set = masks(Completeness::Complete);
            let mut raw = frame(rect([-10, -20], [8, 4]));
            let clean = sanitize(&mut raw, &set.lock().unwrap()).unwrap();
            let region = OcrRegion::window(&clean, &tree(clean.image().0.display)).unwrap();
            let (jobs, incoming) = mpsc::sync_channel(0);
            let mut recognizer = if at_mask_lock {
                TextRecognizer::start(|| {
                    Ok(Fake {
                        words: Vec::new(),
                        max: 100,
                    })
                })
                .unwrap()
            } else {
                TextRecognizer {
                    jobs,
                    retired: false,
                }
            };
            let _keep_channel_open = incoming;
            let guard = at_mask_lock.then(|| set.lock().unwrap());
            let (end, ended) = mpsc::channel();
            let deadline = if failure == Failure::Deadline {
                Instant::now() + Duration::from_millis(100)
            } else {
                until()
            };
            std::thread::scope(|scope| {
                let recognizer = &mut recognizer;
                let clean = &clean;
                let job_masks = Arc::clone(&set);
                let result = scope.spawn(move || {
                    recognizer.recognize(clean, region, &job_masks, &ended, deadline)
                });
                std::thread::sleep(Duration::from_millis(20));
                if failure == Failure::Cancelled {
                    end.send(()).unwrap();
                }
                if failure == Failure::Closed {
                    drop(end);
                }
                assert_eq!(result.join().unwrap().unwrap_err(), failure);
            });
            drop(guard);
        }
    }
}

#[test]
fn disconnected_request_and_response_channels_are_closed() {
    let set = masks(Completeness::Complete);
    let mut raw = frame(rect([-10, -20], [8, 4]));
    let clean = sanitize(&mut raw, &set.lock().unwrap()).unwrap();
    let region = OcrRegion::window(&clean, &tree(clean.image().0.display)).unwrap();
    let (_end, ended) = mpsc::channel();
    let (jobs, incoming) = mpsc::sync_channel(1);
    drop(incoming);
    let mut recognizer = TextRecognizer {
        jobs,
        retired: false,
    };
    assert_eq!(
        recognizer
            .recognize(&clean, region, &set, &ended, until())
            .unwrap_err(),
        Failure::Closed
    );
    let (jobs, incoming) = mpsc::sync_channel::<Job>(1);
    let thread = std::thread::spawn(move || {
        drop(incoming.recv().unwrap());
    });
    let mut recognizer = TextRecognizer {
        jobs,
        retired: false,
    };
    assert_eq!(
        recognizer
            .recognize(&clean, region, &set, &ended, until())
            .unwrap_err(),
        Failure::Closed
    );
    thread.join().unwrap();
}

#[test]
fn debug_redacts_outer_result_job_bitmap_and_native_words() {
    let marker = "SENTINEL PRIVATE";
    let words = vec![word(marker, [0.0, 0.0, 1.0, 1.0])];
    let set = masks(Completeness::Complete);
    let output = run(words, &set, rect([-10, -20], [8, 4]), 100);
    let (reply, _response) = mpsc::sync_channel(1);
    let job = Job {
        bitmap: Bitmap {
            bgra: image::RgbaImage::from_raw(4, 1, b"SENTINEL PRIVATE!".to_vec()).unwrap(),
        },
        region: OcrRegion {
            rect: rect([0, 0], [4, 1]),
            capture: CaptureId(1),
            _source: Source::Window,
        },
        masks: Vec::new(),
        output,
        stop: Stop {
            aborted: Arc::new(AtomicBool::new(false)),
            until: until(),
        },
        reply,
    };
    let native = word(marker, [0.0, 0.0, 1.0, 1.0]);
    let debug = format!(
        "{:?} {:?} {:?} {:?}",
        Ok::<_, Failure>(&job.output),
        job,
        native,
        job.output.words[0]
    );
    assert!(!debug.contains(marker));
    assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
    assert!(!debug.contains("83, 69, 78, 84"));
}
