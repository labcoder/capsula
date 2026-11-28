//! The GPU half of the stage side: one Direct3D 12 device, a budget of the resource bytes it
//! requests, and meshes uploaded as [`Built`] geometries. Nothing here waits on the GPU: wgpu
//! answers its native futures at once (docs/development/render.md).
//!
//! Known limit: this crate's calls avoid every wgpu entry that reaches wgpu's fatal handler, but
//! wgpu 29.0.4 itself unwraps some Direct3D failures, as when it begins its pending-writes
//! command list at open and on the first queued write after a submission. A device removed at
//! those moments can panic inside wgpu, and a release build aborts on panic. This must be closed
//! before a shell hosts the renderer.

use std::{
    fmt,
    ops::Range,
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context, Poll, Waker},
};

use super::{GeometryId, Mesh, Piece, Record, Vertex, mesh::range};
use crate::limits::{INPUT_OBJECTS, INPUT_PRIMS, UPLOADED};

/// The image format drawing needs, as GPUI's Windows renderer draws paths.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
/// Released charges a budget tracks one by one; past them it merges into its newest.
const WAITING: usize = 8;

/// Why the GPU side can't do what it was asked: a fixed kind with no content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GpuFailure {
    /// No Direct3D 12 back end in this build, or no adapter of the kind asked for.
    #[error("no Direct3D 12 adapter")]
    NoAdapter,
    /// The adapter lacks what drawing needs, or wgpu didn't answer at once.
    #[error("the GPU can't open the device drawing needs")]
    NoDevice,
    /// The request passes the budget, or every geometry slot is held.
    #[error("the request passes the renderer's GPU budget")]
    Budget,
    /// The GPU refused to create or stage a resource.
    #[error("the GPU refused a resource")]
    Refused,
    /// The mesh holds no finished geometry.
    #[error("the mesh holds no finished geometry")]
    Unfinished,
    /// The device is lost: every later upload answers this, and releases still take back their
    /// slots.
    #[error("the GPU device was lost")]
    Lost,
}

/// One Direct3D 12 device and the geometries uploaded to it.
pub struct Gpu {
    id: u64,
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Set once, by the loss callback, the uncaptured-error handler or an upload whose write wgpu
    /// refused with no error to say why; never cleared.
    lost: Arc<AtomicBool>,
    /// The newest submission the GPU has finished. Nothing submits yet, so it stays 0.
    retired: Arc<AtomicU64>,
    /// Submissions made so far; the next one carries every write queued since.
    submitted: u64,
    budget: Budget,
    /// Range tables for the geometries not uploaded, reserved at open.
    tables: Vec<Tables>,
    #[cfg(all(test, windows))]
    on_cpu: bool,
    #[cfg(all(test, windows))]
    refuse_second: bool,
}

impl Gpu {
    /// Opens the default Direct3D 12 adapter with `budget` bytes of resources to request.
    ///
    /// # Errors
    /// [`GpuFailure::NoAdapter`] without Direct3D 12 or an adapter; [`GpuFailure::NoDevice`]
    /// when the adapter lacks 4x multisampling and resolve of the image format, sample-rate
    /// shading or vertex-stage storage buffers.
    pub fn open(budget: u64) -> Result<Self, GpuFailure> {
        Self::open_with(&wgpu::RequestAdapterOptions::default(), budget)
    }

    fn open_with(
        options: &wgpu::RequestAdapterOptions<'_, '_>,
        budget: u64,
    ) -> Result<Self, GpuFailure> {
        static DEVICES: AtomicU64 = AtomicU64::new(0);
        if !wgpu::Instance::enabled_backend_features().contains(wgpu::Backends::DX12) {
            return Err(GpuFailure::NoAdapter);
        }
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            flags: wgpu::InstanceFlags::empty(),
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = ready(instance.request_adapter(options))
            .ok_or(GpuFailure::NoDevice)?
            .map_err(|_| GpuFailure::NoAdapter)?;
        let format = adapter.get_texture_format_features(FORMAT);
        let needs =
            wgpu::DownlevelFlags::MULTISAMPLED_SHADING | wgpu::DownlevelFlags::VERTEX_STORAGE;
        let samples = wgpu::TextureFormatFeatureFlags::MULTISAMPLE_X4
            | wgpu::TextureFormatFeatureFlags::MULTISAMPLE_RESOLVE;
        if !adapter.get_downlevel_capabilities().flags.contains(needs)
            || !format.flags.contains(samples)
            || !format
                .allowed_usages
                .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
        {
            return Err(GpuFailure::NoDevice);
        }
        let (device, queue) = ready(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .ok_or(GpuFailure::NoDevice)?
            .map_err(|_| GpuFailure::NoDevice)?;
        let lost = Arc::new(AtomicBool::new(false));
        let latch = Arc::clone(&lost);
        device.set_device_lost_callback(move |_, _| latch.store(true, Ordering::Release));
        let latch = Arc::clone(&lost);
        device.on_uncaptured_error(Arc::new(move |_| latch.store(true, Ordering::Release)));
        let mut tables = Vec::with_capacity(UPLOADED);
        tables.extend((0..UPLOADED).map(|_| Tables::new()));
        Ok(Self {
            id: DEVICES.fetch_add(1, Ordering::Relaxed),
            #[cfg(all(test, windows))]
            on_cpu: adapter.get_info().device_type == wgpu::DeviceType::Cpu,
            #[cfg(all(test, windows))]
            refuse_second: false,
            device,
            queue,
            lost,
            retired: Arc::new(AtomicU64::new(0)),
            submitted: 0,
            budget: Budget::new(budget),
            tables,
        })
    }

    /// Uploads the geometry `mesh` last finished, with its own copy of the mesh's range tables,
    /// so the mesh can build the next geometry while this one stays.
    ///
    /// # Errors
    /// [`GpuFailure::Unfinished`] for a mesh with no finished geometry; [`GpuFailure::Budget`]
    /// past the budget or with every geometry slot held; [`GpuFailure::Refused`] when the GPU
    /// refuses a buffer or a write; [`GpuFailure::Lost`]. The slot comes back at once then, and
    /// the charge too unless a write was already queued, when it waits like a released one.
    ///
    /// The bytes are staged in memory wgpu allocates for the copy, outside the budget, and
    /// reach the GPU with the device's next submission.
    pub fn upload(&mut self, mesh: &Mesh) -> Result<Built, GpuFailure> {
        self.live()?;
        let geometry = mesh.geometry().ok_or(GpuFailure::Unfinished)?;
        let sizes = [
            bytes(mesh.vertices().len(), VERTEX_WORDS * 4)?,
            bytes(mesh.indices().len(), 4)?,
            bytes(mesh.pieces().len(), PIECE_WORDS * 4)?,
        ];
        let total = sizes
            .iter()
            .try_fold(0_u64, |sum, size| sum.checked_add(*size))
            .ok_or(GpuFailure::Budget)?;
        let mut tables = self.tables.pop().ok_or(GpuFailure::Budget)?;
        if let Err(failure) = self.reserve(total) {
            self.tables.push(tables);
            return Err(failure);
        }
        let carried = self.submitted + 1;
        let buffer = |size, usage| {
            self.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let copy = wgpu::BufferUsages::COPY_DST;
        #[cfg(all(test, windows))]
        let second = if self.refuse_second {
            wgpu::BufferUsages::empty()
        } else {
            copy
        };
        #[cfg(not(all(test, windows)))]
        let second = copy;
        let mut queued = false;
        let made = scoped(&self.device, || {
            let made = [
                buffer(sizes[0], wgpu::BufferUsages::VERTEX | copy),
                buffer(sizes[1], wgpu::BufferUsages::INDEX | second),
                buffer(sizes[2], wgpu::BufferUsages::STORAGE | copy),
            ];
            let [vertices, indices, pieces] = &made;
            let staged = stage::<VERTEX_WORDS>(
                &self.queue,
                vertices,
                mesh.vertices().len(),
                mesh.vertices().iter().flat_map(vertex_words),
            )
            .and_then(|()| {
                queued = true;
                stage::<1>(
                    &self.queue,
                    indices,
                    mesh.indices().len(),
                    mesh.indices().iter().copied(),
                )
            })
            .and_then(|()| {
                stage::<PIECE_WORDS>(
                    &self.queue,
                    pieces,
                    mesh.pieces().len(),
                    mesh.pieces().iter().flat_map(piece_words),
                )
            });
            (made, staged.is_some())
        });
        let failure = match made {
            Ok(([vertices, indices, pieces], true)) => match self.live() {
                Ok(()) => {
                    let [starts, objects] = mesh.tables();
                    tables.starts.extend_from_slice(starts);
                    tables.objects.extend_from_slice(objects);
                    return Ok(Built {
                        device: self.id,
                        geometry,
                        vertices,
                        indices,
                        pieces,
                        tables,
                        bytes: total,
                        carried,
                    });
                }
                Err(lost) => lost,
            },
            Ok((_, false)) => {
                // wgpu drops a lost device's errors, so a write refused with both scopes clean
                // has no other explanation.
                self.lost.store(true, Ordering::Release);
                GpuFailure::Lost
            }
            Err(failure) => self.live().err().unwrap_or(failure),
        };
        self.tables.push(tables);
        if queued {
            self.release(total, carried);
        } else {
            self.budget.refund(total);
        }
        Err(failure)
    }

    /// Gives back `built`'s geometry slot at once, and its charge once the submission that
    /// carries its writes has retired: wgpu holds its buffers until then. Nothing submits yet,
    /// so the charge stays held.
    ///
    /// # Errors
    /// Hands `built` back, unreleased, when another device uploaded it.
    #[expect(
        clippy::result_large_err,
        reason = "a refused geometry goes back to its caller whole, on a path that never repeats"
    )]
    pub fn release_built(&mut self, built: Built) -> Result<(), Built> {
        if built.device != self.id {
            return Err(built);
        }
        let mut tables = built.tables;
        tables.starts.clear();
        tables.objects.clear();
        self.tables.push(tables);
        self.release(built.bytes, built.carried);
        Ok(())
    }

    /// Refunds `bytes` once submission `last` has retired.
    fn release(&mut self, bytes: u64, last: u64) {
        self.budget
            .release(bytes, last, self.retired.load(Ordering::Acquire));
    }

    /// Charges `bytes`, first taking back what retired submissions released.
    fn reserve(&mut self, bytes: u64) -> Result<(), GpuFailure> {
        self.budget.reclaim(self.retired.load(Ordering::Acquire));
        self.budget.reserve(bytes)
    }

    fn live(&self) -> Result<(), GpuFailure> {
        if self.lost.load(Ordering::Acquire) {
            return Err(GpuFailure::Lost);
        }
        Ok(())
    }
}

/// Counts only: nothing here is the drawing's.
impl fmt::Debug for Gpu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gpu")
            .field("budget", &self.budget)
            .field("free_tables", &self.tables.len())
            .finish_non_exhaustive()
    }
}

/// One geometry on the GPU, with the range tables that resolve its records. It is provisional:
/// its buffers were created and its writes handed to wgpu with no error and no loss seen, not
/// confirmed on the GPU. wgpu reports nothing when a device is lost under a write, so such a
/// loss shows only once it is latched, when every later upload answers `Lost`, and nothing of
/// this geometry is drawn. Hand it back with [`Gpu::release_built`]: one dropped instead keeps
/// its charge and its table slot for the device's life.
pub struct Built {
    device: u64,
    geometry: GeometryId,
    #[expect(dead_code, reason = "drawing reads the uploaded buffers")]
    vertices: wgpu::Buffer,
    #[expect(dead_code, reason = "drawing reads the uploaded buffers")]
    indices: wgpu::Buffer,
    #[expect(dead_code, reason = "drawing reads the uploaded buffers")]
    pieces: wgpu::Buffer,
    tables: Tables,
    bytes: u64,
    /// The submission that carries its writes.
    carried: u64,
}

impl Built {
    /// Bytes one geometry slot's range tables reserve.
    pub const TABLE_BYTES: usize =
        size_of::<Tables>() + (INPUT_PRIMS + 1 + INPUT_OBJECTS) * size_of::<u32>();

    /// The geometry it holds.
    pub fn geometry(&self) -> GeometryId {
        self.geometry
    }

    /// The indices a record draws from this geometry, if it names primitives of one of its
    /// objects: [`Mesh::range`] as it was when this geometry was uploaded.
    pub fn range(&self, record: &Record) -> Option<Range<u32>> {
        range(&self.tables.starts, &self.tables.objects, record)
    }

    /// The bytes one slot's range tables really reserve.
    #[cfg(test)]
    pub(crate) fn reserved_tables() -> usize {
        let tables = Tables::new();
        size_of::<Tables>()
            + (tables.starts.capacity() + tables.objects.capacity()) * size_of::<u32>()
    }
}

/// Which geometry and its byte charge: nothing of the drawing.
impl fmt::Debug for Built {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Built")
            .field("geometry", &self.geometry)
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

/// A geometry's range tables, as its mesh's: each primitive's first index then the end, and
/// each object's first primitive.
struct Tables {
    starts: Vec<u32>,
    objects: Vec<u32>,
}

impl Tables {
    fn new() -> Self {
        Self {
            starts: Vec::with_capacity(INPUT_PRIMS + 1),
            objects: Vec::with_capacity(INPUT_OBJECTS),
        }
    }
}

/// A budget of requested resource bytes: what this device's resources asked wgpu for, not
/// measured video memory. A released charge waits for the submission that last needed it.
#[derive(Debug)]
struct Budget {
    limit: u64,
    charged: u64,
    /// `(submission, bytes)` released but maybe still held by wgpu.
    waiting: [(u64, u64); WAITING],
    len: usize,
}

impl Budget {
    fn new(limit: u64) -> Self {
        Self {
            limit,
            charged: 0,
            waiting: [(0, 0); WAITING],
            len: 0,
        }
    }

    fn reserve(&mut self, bytes: u64) -> Result<(), GpuFailure> {
        self.charged = self
            .charged
            .checked_add(bytes)
            .filter(|total| *total <= self.limit)
            .ok_or(GpuFailure::Budget)?;
        Ok(())
    }

    fn refund(&mut self, bytes: u64) {
        self.charged = self.charged.saturating_sub(bytes);
    }

    /// Refunds `bytes` at once when submission `last` has retired; otherwise waits for it,
    /// merged into the newest wait when every one is taken, which only holds the merged charge
    /// longer.
    fn release(&mut self, bytes: u64, last: u64, retired: u64) {
        if last <= retired {
            return self.refund(bytes);
        }
        if self.len == WAITING {
            let newest = &mut self.waiting[WAITING - 1];
            *newest = (newest.0.max(last), newest.1 + bytes);
            return;
        }
        self.waiting[self.len] = (last, bytes);
        self.len += 1;
    }

    fn reclaim(&mut self, retired: u64) {
        let mut kept = 0;
        for at in 0..self.len {
            let (last, bytes) = self.waiting[at];
            if last <= retired {
                self.refund(bytes);
            } else {
                self.waiting[kept] = (last, bytes);
                kept += 1;
            }
        }
        self.len = kept;
    }
}

/// Words per encoded vertex: position, along, skip, reveal, weight and piece.
const VERTEX_WORDS: usize = 7;
/// Words per encoded piece: colour, opacity, length, flags and two of padding.
const PIECE_WORDS: usize = 8;
/// Piece flags, as drawing reads them.
const DASHED: u32 = 1;
const REVEALED: u32 = 2;
const RETRACE: u32 = 4;

/// A vertex as drawing reads it, field by field.
fn vertex_words(vertex: &Vertex) -> [u32; VERTEX_WORDS] {
    let [x, y] = vertex.at;
    let [x, y, along, skip, reveal, weight] = [
        x,
        y,
        vertex.along,
        vertex.skip,
        vertex.reveal,
        vertex.weight,
    ]
    .map(f32::to_bits);
    [x, y, along, skip, reveal, weight, vertex.piece]
}

/// A piece as drawing reads it: its paint's channels from 0 to 1, opacity, length, flags.
fn piece_words(piece: &Piece) -> [u32; PIECE_WORDS] {
    let [red, green, blue] = channels(piece.color.rgb).map(f32::to_bits);
    let flags = [
        (piece.dashed, DASHED),
        (piece.revealed, REVEALED),
        (piece.retrace, RETRACE),
    ]
    .into_iter()
    .filter_map(|(set, flag)| set.then_some(flag))
    .sum();
    [
        red,
        green,
        blue,
        piece.color.opacity.to_bits(),
        piece.length.to_bits(),
        flags,
        0,
        0,
    ]
}

/// Packed `0xRRGGBB` as red, green and blue from 0 to 1.
fn channels(rgb: u32) -> [f32; 3] {
    let [_, red, green, blue] = rgb.to_be_bytes();
    [red, green, blue].map(|channel| f32::from(channel) / 255.0)
}

/// Bytes a buffer of `len` items of `each` bytes requests: at least one item, so no buffer
/// or binding is empty.
fn bytes(len: usize, each: usize) -> Result<u64, GpuFailure> {
    len.max(1)
        .checked_mul(each)
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(GpuFailure::Budget)
}

/// Stages `words` for the start of `buffer` and zeros for the rest. Dropping the view queues
/// the copy; none answers when wgpu refuses the write or the device is lost.
fn stage<const N: usize>(
    queue: &wgpu::Queue,
    buffer: &wgpu::Buffer,
    items: usize,
    words: impl Iterator<Item = u32>,
) -> Option<()> {
    // shortcut: wgpu 29.0.4 unwraps a failed Direct3D command list when the first write after a
    // submission begins its pending writes, so a removed device can panic here; close it before
    // a shell hosts the renderer.
    let mut view = queue.write_buffer_with(buffer, 0, wgpu::BufferSize::new(buffer.size())?)?;
    let (head, mut rest) = view.slice(..).split_at(items * N * 4);
    head.into_chunks::<4>()
        .0
        .write_iter(words.map(u32::to_le_bytes));
    rest.fill(0);
    Some(())
}

/// Runs `create` in error scopes for validation and memory.
///
/// # Errors
/// [`GpuFailure::Refused`] when either caught an error, [`GpuFailure::NoDevice`] when wgpu
/// didn't answer at once.
fn scoped<T>(device: &wgpu::Device, create: impl FnOnce() -> T) -> Result<T, GpuFailure> {
    let memory = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    let valid = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let made = create();
    match (ready(valid.pop()), ready(memory.pop())) {
        (Some(None), Some(None)) => Ok(made),
        (Some(_), Some(_)) => Err(GpuFailure::Refused),
        _ => Err(GpuFailure::NoDevice),
    }
}

/// A native wgpu future's answer: they are ready when made, so one poll reads it. A future
/// still pending answers none, and the caller fails closed rather than spin.
fn ready<T>(future: impl Future<Output = T>) -> Option<T> {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(answer) => Some(answer),
        Poll::Pending => None,
    }
}

#[cfg(all(test, windows))]
impl Gpu {
    /// Opens Direct3D 12's software adapter, as every device test does.
    pub(crate) fn software(budget: u64) -> Result<Self, GpuFailure> {
        let options = wgpu::RequestAdapterOptions {
            force_fallback_adapter: true,
            ..Default::default()
        };
        Self::open_with(&options, budget)
    }

    pub(crate) fn on_cpu(&self) -> bool {
        self.on_cpu
    }

    pub(crate) fn charged(&self) -> u64 {
        self.budget.charged
    }

    pub(crate) fn free_tables(&self) -> usize {
        self.tables.len()
    }

    /// Makes every later upload's second buffer refuse its write.
    pub(crate) fn refuse_second(&mut self) {
        self.refuse_second = true;
    }

    /// Whether the loss flag is set, without polling.
    pub(crate) fn flagged(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    pub(crate) fn destroy(&self) {
        self.device.destroy();
    }

    /// One non-blocking poll; whether the device is lost afterwards.
    pub(crate) fn step(&self) -> bool {
        drop(self.device.poll(wgpu::PollType::Poll));
        self.lost.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::{Budget, GpuFailure, WAITING};

    #[test]
    fn a_charge_fits_to_the_byte_and_refuses_one_past_or_an_overflow() {
        let mut budget = Budget::new(1_000);
        budget.reserve(400).expect("it fits");
        assert_eq!(
            budget.reserve(601),
            Err(GpuFailure::Budget),
            "one byte past"
        );
        assert_eq!(budget.charged, 400, "a refusal charges nothing");
        budget.reserve(600).expect("exactly the rest fits");
        let mut full = Budget::new(u64::MAX);
        full.reserve(u64::MAX - 1).expect("it fits");
        assert_eq!(
            full.reserve(2),
            Err(GpuFailure::Budget),
            "the sum overflows"
        );
        assert_eq!(full.charged, u64::MAX - 1);
    }

    #[test]
    fn a_released_charge_waits_for_its_last_submission_and_never_unbounded() {
        let mut budget = Budget::new(10_000);
        budget.reserve(3_000).expect("it fits");
        budget.release(100, 0, 0);
        assert_eq!(budget.charged, 2_900, "never submitted: refunded at once");
        budget.release(200, 5, 4);
        assert_eq!(budget.charged, 2_900, "submission 5 still runs");
        budget.reclaim(4);
        assert_eq!(budget.charged, 2_900);
        budget.reclaim(5);
        assert_eq!(budget.charged, 2_700, "refunded once 5 retires");
        for last in 1..=10 {
            budget.release(10, 20 + last, 0);
        }
        assert_eq!(
            budget.len, WAITING,
            "past its slots a wait merges into the newest"
        );
        budget.reclaim(28);
        assert_eq!(
            budget.charged, 2_630,
            "the seven waits for 21 to 27 are refunded"
        );
        budget.reclaim(30);
        assert_eq!(
            budget.charged, 2_600,
            "the merged wait holds until the newest retires"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn without_direct3d_12_in_the_build_there_is_no_adapter() {
        assert_eq!(
            super::Gpu::open(u64::MAX).err(),
            Some(GpuFailure::NoAdapter)
        );
    }
}
