//! Regional OCR policy over copied sanitized pixels; native objects stay on one thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, TryLockError, mpsc};
use std::time::{Duration, Instant};

use kumochi_protocol::geometry::PhysicalRect;

use super::elements::{Availability, ElementTree};
use super::encode::CaptureTransform;
use super::mask::{Completeness, MaskSink};
use super::sanitize::{SanitizedFrame, intersection};
use super::wire::CaptureId;

const WORDS: usize = 2_000;
const BYTES: usize = 32_768;
const POLL: Duration = Duration::from_millis(1);

/// Fewer named elements than this and the control view likely missed the content.
pub(crate) const NAMED_MIN: usize = 20;
/// The window class that hosts a Chromium page's rendered content.
pub(crate) const PAGE_CLASS: &str = "Chrome_RenderWidgetHostHWND";

pub(crate) fn should_recognize(named_count: usize, focused_hwnd_class: &str) -> bool {
    named_count < NAMED_MIN || focused_hwnd_class == PAGE_CLASS
}

#[derive(Clone, Copy, Debug)]
enum Source {
    FocusCrop,
    Window,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct OcrRegion {
    rect: PhysicalRect,
    capture: CaptureId,
    _source: Source,
}

impl OcrRegion {
    pub(crate) fn focus(
        frame: &SanitizedFrame<'_>,
        transform: CaptureTransform,
    ) -> Result<Self, Failure> {
        let (meta, _) = frame.image();
        if transform.display != meta.display {
            return Err(Failure::Invalid);
        }
        Self::clipped(
            frame,
            transform.crop.ok_or(Failure::Invalid)?,
            Source::FocusCrop,
        )
    }

    pub(crate) fn window(frame: &SanitizedFrame<'_>, tree: &ElementTree) -> Result<Self, Failure> {
        if tree.capture != frame.image().0.capture || tree.availability != Availability::Read {
            return Err(Failure::Invalid);
        }
        Self::clipped(frame, tree.window, Source::Window)
    }

    fn clipped(
        frame: &SanitizedFrame<'_>,
        rect: PhysicalRect,
        source: Source,
    ) -> Result<Self, Failure> {
        let (meta, _) = frame.image();
        Ok(Self {
            rect: intersection(rect, meta.display).ok_or(Failure::Invalid)?,
            capture: meta.capture,
            _source: source,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Failure {
    #[error("invalid OCR region")]
    Invalid,
    #[error("OCR masks pending")]
    Pending,
    #[error("OCR superseded")]
    Superseded,
    #[error("OCR unavailable")]
    Unavailable,
    #[error("OCR cancelled")]
    Cancelled,
    #[error("OCR channel closed")]
    Closed,
    #[error("OCR deadline")]
    Deadline,
}

pub(crate) struct Word {
    pub(crate) text: String,
    pub(crate) rect: PhysicalRect,
}
pub(crate) struct OcrWords {
    pub(crate) capture: CaptureId,
    pub(crate) generation: u32,
    pub(crate) completeness: Completeness,
    pub(crate) words: Vec<Word>,
    pub(crate) truncated: bool,
}
pub(crate) struct Bitmap {
    pub(crate) bgra: image::RgbaImage,
}
pub(crate) struct NativeWord {
    pub(crate) text: String,
    pub(crate) bounds: [f64; 4],
}

impl std::fmt::Debug for Word {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Word")
            .field("bytes", &self.text.len())
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for OcrWords {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OcrWords")
            .field("words", &self.words.len())
            .field("truncated", &self.truncated)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for Bitmap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bitmap")
            .field("size", &self.bgra.dimensions())
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for NativeWord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeWord")
            .field("bytes", &self.text.len())
            .finish_non_exhaustive()
    }
}

pub(crate) struct Stop {
    aborted: Arc<AtomicBool>,
    until: Instant,
}
impl Stop {
    pub(crate) fn check(&self) -> Result<(), Failure> {
        if self.aborted.load(Ordering::Acquire) {
            return Err(Failure::Cancelled);
        }
        if Instant::now() >= self.until {
            return Err(Failure::Deadline);
        }
        Ok(())
    }
}
struct Abort(Arc<AtomicBool>);
impl Drop for Abort {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

pub(crate) trait Engine {
    fn max_dimension(&self) -> u32;
    fn recognize(
        &mut self,
        image: &Bitmap,
        stop: &Stop,
        emit: &mut dyn FnMut(NativeWord) -> bool,
    ) -> Result<(), Failure>;
}

struct Job {
    bitmap: Bitmap,
    region: OcrRegion,
    masks: Vec<PhysicalRect>,
    output: OcrWords,
    stop: Stop,
    reply: mpsc::SyncSender<Result<OcrWords, Failure>>,
}
impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("bitmap", &self.bitmap)
            .field("output", &self.output)
            .finish_non_exhaustive()
    }
}

pub(crate) struct TextRecognizer {
    jobs: mpsc::SyncSender<Job>,
    retired: bool,
}
impl std::fmt::Debug for TextRecognizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TextRecognizer(..)")
    }
}
impl TextRecognizer {
    #[cfg(windows)]
    pub(crate) fn new() -> Result<Self, Failure> {
        Self::start(crate::platform::open_ocr)
    }

    fn start<E: Engine + 'static>(
        open: impl FnOnce() -> Result<E, Failure> + Send + 'static,
    ) -> Result<Self, Failure> {
        let (jobs, incoming) = mpsc::sync_channel::<Job>(1);
        std::thread::Builder::new()
            .name("screen-ocr".into())
            .spawn(move || {
                let mut engine = open();
                while let Ok(mut job) = incoming.recv() {
                    let result = match &mut engine {
                        Ok(engine) => recognize(engine, &mut job).map(|()| job.output),
                        Err(error) => Err(*error),
                    };
                    if let Err(error) = &result {
                        engine = Err(*error);
                    }
                    if job.reply.try_send(result).is_err() {
                        break;
                    }
                }
            })
            .map_err(|_| Failure::Unavailable)?;
        Ok(Self {
            jobs,
            retired: false,
        })
    }

    pub(crate) fn recognize(
        &mut self,
        frame: &SanitizedFrame<'_>,
        region: OcrRegion,
        masks: &MaskSink,
        end: &mpsc::Receiver<()>,
        until: Instant,
    ) -> Result<OcrWords, Failure> {
        if self.retired {
            return Err(Failure::Closed);
        }
        let stop = Stop {
            aborted: Arc::new(AtomicBool::new(false)),
            until,
        };
        let _abort = Abort(Arc::clone(&stop.aborted));
        let result = self.request(frame, region, masks, end, &stop);
        if matches!(
            result,
            Err(Failure::Cancelled | Failure::Closed | Failure::Deadline)
        ) {
            self.retired = true;
        }
        result
    }

    fn request(
        &self,
        frame: &SanitizedFrame<'_>,
        region: OcrRegion,
        masks: &MaskSink,
        end: &mpsc::Receiver<()>,
        stop: &Stop,
    ) -> Result<OcrWords, Failure> {
        check(end, stop)?;
        if frame.completeness() == Completeness::Pending {
            return Err(Failure::Pending);
        }
        let (meta, pixels) = frame.image();
        if region.capture != meta.capture
            || intersection(region.rect, meta.display) != Some(region.rect)
        {
            return Err(Failure::Invalid);
        }
        let mut bgra = image::RgbaImage::new(region.rect.size[0], region.rect.size[1]);
        let [left, top] = std::array::from_fn(|i| {
            u32::try_from(i64::from(region.rect.origin[i]) - i64::from(meta.display.origin[i]))
                .expect("clipped region")
        });
        let row_bytes = usize::try_from(region.rect.size[0]).expect("bounded row width") * 4;
        for (y, row) in bgra.as_mut().chunks_exact_mut(row_bytes).enumerate() {
            check(end, stop)?;
            let offset = ((usize::try_from(top).expect("clipped top") + y)
                * usize::try_from(meta.display.size[0]).expect("bounded display width")
                + usize::try_from(left).expect("clipped left"))
                * 4;
            row.copy_from_slice(&pixels[offset..offset + row_bytes]);
        }
        let (reply, response) = mpsc::sync_channel(1);
        let mut job = Job {
            bitmap: Bitmap { bgra },
            region,
            masks: frame.painted().to_vec(),
            output: OcrWords {
                capture: meta.capture,
                generation: frame.generation(),
                completeness: frame.completeness(),
                words: Vec::new(),
                truncated: false,
            },
            stop: Stop {
                aborted: Arc::clone(&stop.aborted),
                until: stop.until,
            },
            reply,
        };
        loop {
            check(end, stop)?;
            match self.jobs.try_send(job) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Full(returned)) => job = returned,
                Err(mpsc::TrySendError::Disconnected(_)) => return Err(Failure::Closed),
            }
            pause(end, stop)?;
        }
        let result = loop {
            check(end, stop)?;
            match response.try_recv() {
                Ok(result) => break result?,
                Err(mpsc::TryRecvError::Disconnected) => return Err(Failure::Closed),
                Err(mpsc::TryRecvError::Empty) => pause(end, stop)?,
            }
        };
        loop {
            check(end, stop)?;
            match masks.try_lock() {
                Ok(current) => {
                    return if current.capture() == result.capture
                        && current.generation() == result.generation
                    {
                        Ok(result)
                    } else {
                        Err(Failure::Superseded)
                    };
                }
                Err(TryLockError::Poisoned(_)) => return Err(Failure::Superseded),
                Err(TryLockError::WouldBlock) => pause(end, stop)?,
            }
        }
    }
}

fn check(end: &mpsc::Receiver<()>, stop: &Stop) -> Result<(), Failure> {
    stop.check()?;
    match end.try_recv() {
        Ok(()) => Err(Failure::Cancelled),
        Err(mpsc::TryRecvError::Disconnected) => Err(Failure::Closed),
        Err(mpsc::TryRecvError::Empty) => Ok(()),
    }
}
fn pause(end: &mpsc::Receiver<()>, stop: &Stop) -> Result<(), Failure> {
    check(end, stop)?;
    match end.recv_timeout(POLL.min(stop.until.saturating_duration_since(Instant::now()))) {
        Ok(()) => Err(Failure::Cancelled),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(Failure::Closed),
        Err(mpsc::RecvTimeoutError::Timeout) => check(end, stop),
    }
}

fn recognize(engine: &mut dyn Engine, job: &mut Job) -> Result<(), Failure> {
    job.stop.check()?;
    let max = engine.max_dimension();
    if max == 0 {
        return Err(Failure::Unavailable);
    }
    let [w, h] = job.region.rect.size;
    let longest = w.max(h);
    let size = if longest > max {
        [w, h].map(|side| (side * max / longest).max(1))
    } else {
        [w, h]
    };
    // shortcut: small text in a region wider than the OCR maximum may be missed; tile if A6's numbers or a user show misses
    let scaled = (size != [w, h]).then(|| Bitmap {
        bgra: image::imageops::resize(
            &job.bitmap.bgra,
            size[0],
            size[1],
            image::imageops::FilterType::Triangle,
        ),
    });
    let output = &mut job.output;
    let mut bytes = 0;
    engine.recognize(
        scaled.as_ref().unwrap_or(&job.bitmap),
        &job.stop,
        &mut |word| {
            if word.text.is_empty() {
                return true;
            }
            let Some(rect) = physical_bounds(word.bounds, job.region.rect, size) else {
                return true;
            };
            if job
                .masks
                .iter()
                .any(|mask| intersection(rect, *mask).is_some())
            {
                return true;
            }
            if output.words.len() == WORDS {
                output.truncated = true;
                return false;
            }
            let available = BYTES - bytes;
            let mut text = word.text;
            if text.len() > available {
                let mut boundary = available;
                while !text.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                text.truncate(boundary);
                output.truncated = true;
            }
            bytes += text.len();
            if !text.is_empty() {
                output.words.push(Word { text, rect });
            }
            !output.truncated
        },
    )?;
    job.stop.check()?;
    Ok(())
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "finite clipped bitmap edges round outward into bounded physical pixels"
)]
fn physical_bounds(bounds: [f64; 4], region: PhysicalRect, size: [u32; 2]) -> Option<PhysicalRect> {
    if !bounds.iter().all(|v| v.is_finite()) || bounds[2] <= 0.0 || bounds[3] <= 0.0 {
        return None;
    }
    let mut rect = region;
    for axis in 0..2 {
        let scale = f64::from(region.size[axis]) / f64::from(size[axis]);
        let left = (bounds[axis] * scale)
            .floor()
            .clamp(0.0, f64::from(region.size[axis]));
        let right = ((bounds[axis] + bounds[axis + 2]) * scale)
            .ceil()
            .clamp(0.0, f64::from(region.size[axis]));
        if right <= left {
            return None;
        }
        rect.origin[axis] = i32::try_from(i64::from(region.origin[axis]) + left as i64).ok()?;
        rect.size[axis] = (right - left) as u32;
    }
    Some(rect)
}

#[cfg(test)]
#[path = "ocr/tests.rs"]
mod tests;
