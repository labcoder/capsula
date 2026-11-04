//! `kumochi --gate-observer=<file>`: the gate harness's record of what the dock really did. Each
//! hit-mask commit logs the exact rows it drew and owned, each dock paint its frame, each frame's
//! completion its dispatch on the proxy, and each tagged click its receipt, all stamped with the
//! performance counter. The UI thread only moves fixed-size records into a preallocated bounded
//! queue; a writer thread formats and flushes them. A dropped record, a shape taller than
//! [`ROW_CAP`] or a failed post is counted in the close record, and any of them makes the run
//! invalid for the gate. Records carry geometry and times, never text or another app's content.

use std::cell::Cell;
use std::fmt;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::rc::Rc;
use std::thread::JoinHandle;

use async_channel::{Receiver, Sender};
use gpui_kit::{App, Bounds, Global, Pixels, ScaledPixels, Window, px};
use kumochi_protocol::constants::{INPUT_SESSION_MARKER, OBSERVER_REPAINT_MESSAGE};

use crate::platform::ROW_SPANS;

use super::geometry::{PanelVisibility, Position};
use super::line::LineFrame;
use crate::platform::{self, MaskUpdate, PresenceError, PresenceEvent, ProxyHandle, ProxyNote};

/// Rows one geometry record holds; the line's band at 200 % is under 50.
pub(super) const ROW_CAP: usize = 512;
/// Records the writer may fall behind by; one more is dropped and the run is invalid.
const QUEUE: usize = 256;
/// Frames painted before the proxy exists, while GPUI opens the windows; one more is a failed post.
const PENDING: usize = 8;

/// Drawn spans per row, one per shape: the line with its drip first, then the body with its stub.
const DRAWN_SPANS: usize = 2;
/// One row: [`DRAWN_SPANS`] drawn spans, then [`ROW_SPANS`] owned spans, each `[left, right)`.
/// An empty span has `left == right`; real spans never do.
type Row = [i32; 2 * (DRAWN_SPANS + ROW_SPANS)];
static EMPTY_ROWS: [Row; ROW_CAP] = [[0; 2 * (DRAWN_SPANS + ROW_SPANS)]; ROW_CAP];

/// One commit's exact rows: the line's painted pixels and the band the mask is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Geometry {
    origin_y: i32,
    /// Rows from `origin_y`; more than [`ROW_CAP`] is an overflow, and `spans` holds none.
    rows: usize,
    trimmed: bool,
    spans: [Row; ROW_CAP],
}

impl fmt::Display for Geometry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "origin_y={} rows={} trimmed={}",
            self.origin_y, self.rows, self.trimmed
        )?;
        if self.overflowed() {
            return f.write_str(" overflow");
        }
        f.write_str(" spans=")?;
        for (index, row) in self.spans[..self.rows].iter().enumerate() {
            if index > 0 {
                f.write_str(";")?;
            }
            for (index, [left, right]) in row.as_chunks::<2>().0.iter().enumerate() {
                if index > 0 {
                    f.write_str(",")?;
                }
                write!(f, "{}", Span(*left, *right))?;
            }
        }
        Ok(())
    }
}

impl Geometry {
    pub(super) fn of(frame: &LineFrame) -> Self {
        let [_, top, _, height] = frame.hit;
        let [paint_left, paint_top, paint_width, paint_height] = frame.paint;
        // The line never carries a previous placement, so its mask is always trimmed.
        let mut geometry = Self {
            origin_y: top,
            rows: usize::try_from(height).unwrap_or(0),
            trimmed: true,
            spans: EMPTY_ROWS,
        };
        if geometry.overflowed() {
            return geometry;
        }
        let span = |span: Option<[i32; 2]>| span.unwrap_or([0; 2]);
        for (y, row) in (top..top + height).zip(&mut geometry.spans) {
            let [drawn_left, drawn_right] = span(
                (paint_top..paint_top + paint_height)
                    .contains(&y)
                    .then_some([paint_left, paint_left + paint_width]),
            );
            row[..2].copy_from_slice(&[drawn_left, drawn_right]);
            let owned = row[2 * DRAWN_SPANS..].as_chunks_mut::<2>().0;
            for (at, span_of_row) in owned.iter_mut().zip(frame.row_spans(y)) {
                *at = span(span_of_row);
            }
        }
        geometry
    }

    fn empty() -> Self {
        Self {
            origin_y: 0,
            rows: 0,
            trimmed: true,
            spans: EMPTY_ROWS,
        }
    }

    fn overflowed(&self) -> bool {
        self.rows > ROW_CAP
    }
}

/// The mask state the dock draws from: the accepted epoch and its geometry's id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Drawn {
    pub(super) epoch: u64,
    pub(super) geometry: u64,
}

/// Which geometry the harness got last, and which one the mask accepted last. Only an
/// accepted change advances the epoch.
#[derive(Debug)]
struct Ledger {
    last: Box<Geometry>,
    last_id: u64,
    accepted: Drawn,
}

impl Ledger {
    fn new() -> Self {
        Self {
            last: Box::new(Geometry::empty()),
            last_id: 0,
            accepted: Drawn::default(),
        }
    }

    /// Returns whether `geometry` is new to the harness, its id, and the accepted state after.
    fn commit(&mut self, geometry: &Geometry, result: Commit) -> (bool, u64, Drawn) {
        let new = self.last_id == 0 || *self.last != *geometry;
        if new {
            self.last_id += 1;
            *self.last = *geometry;
        }
        if result != Commit::Failed && self.accepted.geometry != self.last_id {
            self.accepted = Drawn {
                epoch: self.accepted.epoch + 1,
                geometry: self.last_id,
            };
        }
        (new, self.last_id, self.accepted)
    }
}

/// What one mask upload did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Commit {
    Shape,
    Moved,
    Unchanged,
    Failed,
}

impl Commit {
    fn of(update: &Result<MaskUpdate, PresenceError>) -> Self {
        match update {
            Ok(MaskUpdate::Shape) => Self::Shape,
            Ok(MaskUpdate::Moved) => Self::Moved,
            Ok(MaskUpdate::Unchanged) => Self::Unchanged,
            Err(_) => Self::Failed,
        }
    }
}

impl fmt::Display for Commit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Shape => "shape",
            Self::Moved => "moved",
            Self::Unchanged => "unchanged",
            Self::Failed => "failed",
        })
    }
}

/// Which GPUI surface painted a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Surface {
    Dock,
    Panel,
}

impl fmt::Display for Surface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Dock => "dock",
            Self::Panel => "panel",
        })
    }
}

/// What the harness needs to find the dock and check the trace belongs to this run.
#[derive(Clone, Copy, Debug)]
pub(super) struct Startup {
    pub(super) dock_hwnd: usize,
    pub(super) panel_hwnd: usize,
    pub(super) scale: f32,
    pub(super) position: Position,
    pub(super) work_area: [i32; 4],
    pub(super) dock: [i32; 4],
    pub(super) overlay: [i32; 4],
    pub(super) panel: [i32; 4],
}

/// The counts that make a run invalid when any but `commits`, `frames` and `unposted` isn't zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Totals {
    commits: u64,
    frames: u64,
    dropped: u64,
    overflow: u64,
    post_failed: u64,
    /// Frames painted before `start` whose surface never drew again to post them.
    unposted: u64,
}

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "records are fixed-size so the UI thread never allocates; the queue is preallocated"
)]
enum Record {
    Startup {
        startup: Startup,
        pid: u32,
        proxy_hwnd: usize,
        qpc_frequency: i64,
    },
    Geometry {
        id: u64,
        geometry: Geometry,
    },
    Commit {
        seq: u64,
        begin_qpc: i64,
        qpc: i64,
        result: Commit,
        proposal: u64,
        drawn: Drawn,
        proxy: [i32; 4],
    },
    Paint {
        frame: u64,
        qpc: i64,
        surface: Surface,
        hover: u64,
        drawn: Drawn,
        mask_epoch: u64,
    },
    Hover {
        id: u64,
        qpc: i64,
        click_qpc: i64,
        visible: bool,
    },
    Target {
        frame: u64,
        qpc: i64,
        hover: u64,
        rect: [i32; 4],
        inside: bool,
    },
    Drawn {
        frame: u64,
        qpc: i64,
    },
    Click {
        qpc: i64,
        point: [i32; 2],
        mask_epoch: u64,
    },
    Repaint {
        qpc: i64,
    },
    Close(Totals),
}

/// What the UI thread and the proxy's notes share.
#[derive(Debug)]
struct Shared {
    sender: Sender<Record>,
    epoch: Cell<u64>,
    dropped: Cell<u64>,
    /// Cleared at Quit before the final counts are taken, so nothing lands after them.
    active: Cell<bool>,
}

impl Shared {
    fn new(sender: Sender<Record>) -> Self {
        Self {
            sender,
            epoch: Cell::new(0),
            dropped: Cell::new(0),
            active: Cell::new(true),
        }
    }

    fn send(&self, record: Record) {
        if !self.active.get() {
            return;
        }
        if self.sender.try_send(record).is_err() {
            self.dropped.set(self.dropped.get() + 1);
        }
    }
}

/// The observer, kept as a GPUI global only with `--gate-observer`.
#[derive(Debug)]
pub(crate) struct Observer {
    shared: Rc<Shared>,
    writer: Option<JoinHandle<()>>,
    proxy: Option<ProxyHandle>,
    ledger: Ledger,
    totals: Totals,
    /// The latest hover show or hide; the panel's paints name it.
    hover: u64,
    /// Frames painted before `start`, posted from each surface's next frame after it.
    pending: [Option<(Surface, u64)>; PENDING],
}

impl Global for Observer {}

impl Observer {
    /// Creates or truncates `path`, allocates the queue and starts the writer thread. Called
    /// before GPUI starts, so the UI thread never touches the disk.
    ///
    /// # Errors
    /// Fails when the file can't be created or the thread can't start.
    pub(crate) fn prepare(path: Option<&Path>) -> io::Result<Option<Self>> {
        let Some(path) = path else {
            return Ok(None);
        };
        let file = File::create_new(path)?;
        let (sender, receiver) = async_channel::bounded(QUEUE);
        let writer = std::thread::Builder::new()
            .name("gate-observer".to_owned())
            .spawn(move || write(&receiver, file))?;
        Ok(Some(Self {
            shared: Rc::new(Shared::new(sender)),
            writer: Some(writer),
            proxy: None,
            ledger: Ledger::new(),
            totals: Totals::default(),
            hover: 0,
            pending: [None; PENDING],
        }))
    }
}

/// Starts observing the proxy and records where everything is.
pub(super) fn start(
    cx: &mut App,
    native: &platform::PresenceNative,
    events: Sender<PresenceEvent>,
    startup: Startup,
) {
    if !cx.has_global::<Observer>() {
        return;
    }
    let observer = cx.global_mut::<Observer>();
    let shared = Rc::clone(&observer.shared);
    let proxy = native.observe(Box::new(move |note| {
        if !shared.active.get() {
            return;
        }
        match note {
            ProxyNote::Drawn { frame, qpc } => shared.send(Record::Drawn { frame, qpc }),
            ProxyNote::Click { qpc, point } => shared.send(Record::Click {
                qpc,
                point,
                mask_epoch: shared.epoch.get(),
            }),
            ProxyNote::Repaint { qpc } => {
                shared.send(Record::Repaint { qpc });
                if events.try_send(PresenceEvent::Repaint).is_err() {
                    tracing::error!("presence event receiver closed");
                }
            }
        }
    }));
    observer.proxy = Some(proxy);
    observer.shared.send(Record::Startup {
        startup,
        pid: std::process::id(),
        proxy_hwnd: proxy.value(),
        qpc_frequency: platform::performance_frequency(),
    });
}

/// The counter right before a mask upload, so its record bounds the whole call; `0`, with no
/// counter read, without the observer.
pub(super) fn begin(cx: &App) -> i64 {
    if !cx.has_global::<Observer>() {
        return 0;
    }
    platform::performance_counter()
}

/// Records one mask upload of `proposal`, or of an empty mask when it's `None`, into the proxy at `proxy`, begun at `begin_qpc` from
/// [`begin`], right after it returned. Returns the accepted state the dock should name when it
/// draws.
pub(super) fn commit(
    cx: &mut App,
    begin_qpc: i64,
    proposal: Option<&LineFrame>,
    proxy: [i32; 4],
    update: &Result<MaskUpdate, PresenceError>,
) -> Drawn {
    if !cx.has_global::<Observer>() {
        return Drawn::default();
    }
    let qpc = platform::performance_counter();
    let observer = cx.global_mut::<Observer>();
    let result = Commit::of(update);
    let geometry = proposal.map_or_else(Geometry::empty, Geometry::of);
    let (new, id, drawn) = observer.ledger.commit(&geometry, result);
    if new {
        if geometry.overflowed() {
            observer.totals.overflow += 1;
        }
        observer.shared.send(Record::Geometry { id, geometry });
    }
    observer.totals.commits += 1;
    observer.shared.epoch.set(drawn.epoch);
    observer.shared.send(Record::Commit {
        seq: observer.totals.commits,
        begin_qpc,
        qpc,
        result,
        proposal: id,
        drawn,
        proxy,
    });
    drawn
}

/// Records the dock's paint of `drawn` and posts its frame id to the proxy, which stamps it
/// once the draw and present this paint runs inside have returned.
/// The panel paints with `Drawn::default()` and names the hover change it's showing; the dock
/// names no hover. Frame ids count across both surfaces.
pub(super) fn painted(cx: &mut App, surface: Surface, drawn: Drawn) {
    if !cx.has_global::<Observer>() {
        return;
    }
    let qpc = platform::performance_counter();
    let observer = cx.global_mut::<Observer>();
    observer.totals.frames += 1;
    let frame = observer.totals.frames;
    let hover = match surface {
        Surface::Dock => 0,
        Surface::Panel => observer.hover,
    };
    observer.shared.send(Record::Paint {
        frame,
        qpc,
        surface,
        hover,
        drawn,
        mask_epoch: observer.shared.epoch.get(),
    });
    let Some(proxy) = observer.proxy else {
        match observer.pending.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => *slot = Some((surface, frame)),
            None => observer.totals.post_failed += 1,
        }
        return;
    };
    if proxy.post_drawn(frame).is_err() {
        observer.totals.post_failed += 1;
    }
}

/// Posts the frames `surface` painted before `start`. Called from the surface's next frame,
/// ahead of the present that frame makes, so the post is dispatched after GPUI's first present.
/// Their `[paint, drawn]` interval is wider than a live frame's but still bounds the submission.
pub(super) fn flush(cx: &mut App, surface: Surface) {
    if !cx.has_global::<Observer>() {
        return;
    }
    let observer = cx.global_mut::<Observer>();
    let Some(proxy) = observer.proxy else {
        return;
    };
    for slot in &mut observer.pending {
        let Some((painted, frame)) = *slot else {
            continue;
        };
        if painted != surface {
            continue;
        }
        *slot = None;
        if proxy.post_drawn(frame).is_err() {
            observer.totals.post_failed += 1;
        }
    }
}

/// Records a status panel change and optional native click origin as legacy hover `H`.
pub(super) fn hover(cx: &mut App, visibility: PanelVisibility, trigger_qpc: Option<i64>) {
    if !cx.has_global::<Observer>() {
        return;
    }
    let qpc = platform::performance_counter();
    let observer = cx.global_mut::<Observer>();
    observer.hover += 1;
    let id = observer.hover;
    observer.shared.send(Record::Hover {
        id,
        qpc,
        click_qpc: trigger_qpc.unwrap_or(0),
        visible: visibility == PanelVisibility::Visible,
    });
}

/// Records where the status panel's Stop button painted, in physical screen pixels, as the
/// target a harness click aims at. Its frame is the panel's, stamped first in the same draw;
/// `inside` says the bounds lie within the panel's client area.
pub(super) fn target(window: &Window, cx: &mut App, bounds: Bounds<Pixels>) {
    if !cx.has_global::<Observer>() {
        return;
    }
    let qpc = platform::performance_counter();
    let [origin_x, origin_y] = match platform::client_origin(window) {
        Ok(origin) => origin,
        Err(error) => {
            tracing::debug!(%error, "can't read the panel's screen origin");
            return;
        }
    };
    let viewport = window.viewport_size();
    let inside = bounds.origin.x >= px(0.0)
        && bounds.origin.y >= px(0.0)
        && bounds.right() <= viewport.width
        && bounds.bottom() <= viewport.height;
    let scaled = bounds.scale(window.scale_factor());
    let [left, top] = [scaled.origin.x, scaled.origin.y].map(physical);
    let [right, bottom] = [scaled.right(), scaled.bottom()].map(physical);
    let observer = cx.global_mut::<Observer>();
    observer.shared.send(Record::Target {
        frame: observer.totals.frames,
        qpc,
        hover: observer.hover,
        rect: [origin_x + left, origin_y + top, right - left, bottom - top],
        inside,
    });
}

/// A device-pixel coordinate GPUI computed, as the whole pixel it snaps to.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "window coordinates fit i32"
)]
fn physical(value: ScaledPixels) -> i32 {
    value.0.round() as i32
}

/// The writer's last record and thread, to finish off the UI thread at Quit.
#[derive(Debug)]
pub(crate) struct Closing {
    sender: Sender<Record>,
    totals: Totals,
    writer: Option<JoinHandle<()>>,
}

impl Closing {
    /// Queues the close record, waiting for room, then waits for the writer to finish.
    pub(crate) fn finish(self) {
        if self
            .sender
            .send_blocking(Record::Close(self.totals))
            .is_err()
        {
            tracing::warn!("the gate observer's writer stopped early");
        }
        if let Some(writer) = self.writer
            && writer.join().is_err()
        {
            tracing::warn!("the gate observer's writer panicked");
        }
    }
}

/// Ends observation for Quit: removes the observer, so later frames and commits record
/// nothing, and freezes the proxy's notes before the final counts are taken. Frames still in
/// flight keep no `drawn` line. Returns the close record and writer, to finish off the UI thread.
pub(crate) fn release(cx: &mut App) -> Option<Closing> {
    if !cx.has_global::<Observer>() {
        return None;
    }
    let mut observer = cx.remove_global::<Observer>();
    observer.shared.active.set(false);
    let totals = Totals {
        dropped: observer.shared.dropped.get(),
        unposted: observer
            .pending
            .iter()
            .flatten()
            .fold(0, |count, _| count + 1),
        ..observer.totals
    };
    let writer = observer.writer.take()?;
    Some(Closing {
        sender: observer.shared.sender.clone(),
        totals,
        writer: Some(writer),
    })
}

/// The writer thread: formats records as they arrive, flushing whenever it catches up, until
/// the close record.
fn write(receiver: &Receiver<Record>, file: File) {
    let mut out = BufWriter::new(file);
    let mut lines = 0_u64;
    let mut errors = 0_u64;
    while let Ok(record) = receiver.recv_blocking() {
        match line(&mut out, &record) {
            Ok(()) => lines += 1,
            Err(_) => errors += 1,
        }
        if matches!(record, Record::Close(_)) {
            break;
        }
        if receiver.is_empty() && out.flush().is_err() {
            errors += 1;
        }
    }
    let ended = writeln!(out, "end lines={lines} write_errors={errors}").and_then(|()| out.flush());
    if let Err(error) = ended {
        tracing::warn!(%error, "can't finish the gate observer's file");
    }
}

/// `[x, y, width, height]` as `x,y,width,height`.
struct Rect([i32; 4]);

impl fmt::Display for Rect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [x, y, width, height] = self.0;
        write!(f, "{x},{y},{width},{height}")
    }
}

/// A `[left, right)` span as `left:right`, or `-` when empty.
struct Span(i32, i32);

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == self.1 {
            f.write_str("-")
        } else {
            write!(f, "{}:{}", self.0, self.1)
        }
    }
}

fn line(out: &mut impl Write, record: &Record) -> io::Result<()> {
    match *record {
        Record::Startup {
            startup,
            pid,
            proxy_hwnd,
            qpc_frequency,
        } => writeln!(
            out,
            "startup version=2 pid={pid} dock_hwnd={:#x} panel_hwnd={:#x} proxy_hwnd={proxy_hwnd:#x} scale={} \
             position={:?} work_area={} dock={} overlay={} panel={} qpc_frequency={qpc_frequency} \
             row_cap={ROW_CAP} queue={QUEUE} repaint_message={OBSERVER_REPAINT_MESSAGE:#x} \
             marker={INPUT_SESSION_MARKER:#x}",
            startup.dock_hwnd,
            startup.panel_hwnd,
            startup.scale,
            startup.position,
            Rect(startup.work_area),
            Rect(startup.dock),
            Rect(startup.overlay),
            Rect(startup.panel),
        ),
        Record::Geometry { id, ref geometry } => writeln!(out, "geometry id={id} {geometry}"),
        Record::Commit {
            seq,
            begin_qpc,
            qpc,
            result,
            proposal,
            drawn,
            proxy,
        } => writeln!(
            out,
            "commit seq={seq} begin_qpc={begin_qpc} qpc={qpc} result={result} proposal={proposal} epoch={} \
             geometry={} proxy={}",
            drawn.epoch,
            drawn.geometry,
            Rect(proxy)
        ),
        Record::Paint {
            frame,
            qpc,
            surface,
            hover,
            drawn,
            mask_epoch,
        } => writeln!(
            out,
            "paint frame={frame} qpc={qpc} kind={surface} hover={hover} drawn_epoch={} drawn_geometry={} \
             mask_epoch={mask_epoch}",
            drawn.epoch, drawn.geometry
        ),
        Record::Hover {
            id,
            qpc,
            click_qpc,
            visible,
        } => {
            writeln!(
                out,
                "hover id={id} qpc={qpc} visible={visible} click_qpc={click_qpc}"
            )
        }
        Record::Target {
            frame,
            qpc,
            hover,
            rect,
            inside,
        } => writeln!(
            out,
            "target frame={frame} qpc={qpc} kind=stop hover={hover} rect={} inside={inside}",
            Rect(rect)
        ),
        Record::Drawn { frame, qpc } => writeln!(out, "drawn frame={frame} qpc={qpc}"),
        Record::Click {
            qpc,
            point: [x, y],
            mask_epoch,
        } => writeln!(
            out,
            "click qpc={qpc} point={x},{y} mask_epoch={mask_epoch} marker={INPUT_SESSION_MARKER:#x}"
        ),
        Record::Repaint { qpc } => writeln!(out, "repaint qpc={qpc}"),
        Record::Close(totals) => writeln!(
            out,
            "close commits={} frames={} dropped={} overflow={} post_failed={} unposted={}",
            totals.commits,
            totals.frames,
            totals.dropped,
            totals.overflow,
            totals.post_failed,
            totals.unposted
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Commit, Drawn, EMPTY_ROWS, Geometry, Ledger, Observer, ROW_CAP, Record, Shared, Surface,
        Totals, line,
    };
    use crate::presence::line::LineFrame;

    /// A line painted at `paint`, owning `hit`.
    fn frame(paint: [i32; 4], hit: [i32; 4]) -> LineFrame {
        LineFrame { paint, hit }
    }

    fn text(record: &Record) -> String {
        let mut out = Vec::new();
        line(&mut out, record).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn geometry_holds_the_exact_drawn_and_owned_rows() {
        let geometry = Geometry::of(&frame([10, 20, 70, 4], [10, 10, 70, 24]));
        assert_eq!(
            (geometry.origin_y, geometry.rows, geometry.trimmed),
            (10, 24, true)
        );
        assert_eq!(
            geometry.spans[0],
            [0, 0, 0, 0, 10, 80, 0, 0, 0, 0, 0, 0],
            "band, not drawn"
        );
        assert_eq!(geometry.spans[9], [0, 0, 0, 0, 10, 80, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            geometry.spans[10],
            [10, 80, 0, 0, 10, 80, 0, 0, 0, 0, 0, 0],
            "the line's first row"
        );
        assert_eq!(
            geometry.spans[13],
            [10, 80, 0, 0, 10, 80, 0, 0, 0, 0, 0, 0],
            "its last row"
        );
        assert_eq!(geometry.spans[14], [0, 0, 0, 0, 10, 80, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            geometry.spans[23],
            [0, 0, 0, 0, 10, 80, 0, 0, 0, 0, 0, 0],
            "the band's last row"
        );
        assert_eq!(geometry.spans[24], [0; 12], "past the band");
    }

    #[test]
    fn a_band_taller_than_the_row_cap_overflows_instead_of_truncating() {
        let geometry = Geometry::of(&frame([0, 290, 600, 20], [0, 0, 600, 601]));
        assert!(geometry.rows > ROW_CAP);
        assert!(geometry.overflowed());
        assert_eq!(geometry.spans, EMPTY_ROWS, "no partial rows");
        assert_eq!(
            text(&Record::Geometry { id: 4, geometry }),
            "geometry id=4 origin_y=0 rows=601 trimmed=true overflow\n"
        );
    }

    #[test]
    fn only_an_accepted_change_advances_the_epoch() {
        let resting = Geometry::of(&frame([10, 20, 70, 4], [10, 10, 70, 24]));
        let moved = Geometry::of(&frame([300, 20, 70, 4], [300, 10, 70, 24]));
        let mut ledger = Ledger::new();
        let first = Drawn {
            epoch: 1,
            geometry: 1,
        };
        assert_eq!(
            ledger.commit(&resting, Commit::Shape),
            (true, 1, first),
            "initial"
        );
        assert_eq!(
            ledger.commit(&resting, Commit::Unchanged),
            (false, 1, first),
            "unchanged"
        );
        assert_eq!(
            ledger.commit(&moved, Commit::Failed),
            (true, 2, first),
            "a failed upload is logged but owns nothing"
        );
        let second = Drawn {
            epoch: 2,
            geometry: 2,
        };
        assert_eq!(
            ledger.commit(&moved, Commit::Shape),
            (false, 2, second),
            "retried"
        );
        assert_eq!(
            ledger.commit(&resting, Commit::Shape),
            (
                true,
                3,
                Drawn {
                    epoch: 3,
                    geometry: 3
                }
            ),
            "back is a new state"
        );
    }

    #[test]
    fn a_row_writes_both_shapes_and_every_owned_span() {
        let mut geometry = Geometry::empty();
        geometry.rows = 1;
        geometry.spans[0] = [0, 0, 20, 22, 1, 2, 4, 5, 7, 8, 10, 11];
        assert_eq!(
            text(&Record::Geometry { id: 2, geometry }),
            "geometry id=2 origin_y=0 rows=1 trimmed=true spans=-,20:22,1:2,4:5,7:8,10:11\n",
            "the second shape's drawn span and all four owned spans are written, none merged"
        );
    }

    #[test]
    fn records_are_one_line_each() {
        let geometry = Geometry::of(&frame([10, 6, 4, 1], [10, 5, 4, 3]));
        assert_eq!(
            text(&Record::Geometry { id: 1, geometry }),
            "geometry id=1 origin_y=5 rows=3 trimmed=true spans=-,-,10:14,-,-,-;10:14,-,10:14,-,-,-;-,-,10:14,-,-,-\n"
        );
        let drawn = Drawn {
            epoch: 7,
            geometry: 9,
        };
        assert_eq!(
            text(&Record::Commit {
                seq: 3,
                begin_qpc: 990,
                qpc: 1000,
                result: Commit::Unchanged,
                proposal: 9,
                drawn,
                proxy: [-24, 10, 152, 88],
            }),
            "commit seq=3 begin_qpc=990 qpc=1000 result=unchanged proposal=9 epoch=7 geometry=9 proxy=-24,10,152,88\n"
        );
        assert_eq!(
            text(&Record::Paint {
                frame: 5,
                qpc: 1200,
                surface: Surface::Dock,
                hover: 0,
                drawn,
                mask_epoch: 8,
            }),
            "paint frame=5 qpc=1200 kind=dock hover=0 drawn_epoch=7 drawn_geometry=9 mask_epoch=8\n"
        );
        assert_eq!(
            text(&Record::Paint {
                frame: 6,
                qpc: 1250,
                surface: Surface::Panel,
                hover: 2,
                drawn: Drawn::default(),
                mask_epoch: 8,
            }),
            "paint frame=6 qpc=1250 kind=panel hover=2 drawn_epoch=0 drawn_geometry=0 mask_epoch=8\n"
        );
        assert_eq!(
            text(&Record::Hover {
                id: 2,
                qpc: 1240,
                click_qpc: 1230,
                visible: true,
            }),
            "hover id=2 qpc=1240 visible=true click_qpc=1230\n"
        );
        assert_eq!(
            text(&Record::Hover {
                id: 3,
                qpc: 1250,
                click_qpc: 0,
                visible: false
            }),
            "hover id=3 qpc=1250 visible=false click_qpc=0\n"
        );
        assert_eq!(
            text(&Record::Target {
                frame: 6,
                qpc: 1260,
                hover: 2,
                rect: [-1880, 640, 96, 36],
                inside: true,
            }),
            "target frame=6 qpc=1260 kind=stop hover=2 rect=-1880,640,96,36 inside=true\n"
        );
        assert_eq!(
            text(&Record::Click {
                qpc: 1300,
                point: [40, -3],
                mask_epoch: 8,
            }),
            "click qpc=1300 point=40,-3 mask_epoch=8 marker=0x43505331\n"
        );
        assert_eq!(
            text(&Record::Close(Totals {
                commits: 4,
                frames: 5,
                dropped: 1,
                overflow: 0,
                post_failed: 2,
                unposted: 3,
            })),
            "close commits=4 frames=5 dropped=1 overflow=0 post_failed=2 unposted=3\n"
        );
    }

    #[test]
    fn a_frozen_queue_takes_nothing_and_counts_nothing() {
        let (sender, receiver) = async_channel::bounded(1);
        let shared = Shared::new(sender);
        shared.send(Record::Repaint { qpc: 1 });
        shared.send(Record::Repaint { qpc: 2 });
        assert_eq!(shared.dropped.get(), 1);
        shared.active.set(false);
        assert!(matches!(
            receiver.try_recv(),
            Ok(Record::Repaint { qpc: 1 })
        ));
        shared.send(Record::Repaint { qpc: 3 });
        assert!(receiver.is_empty());
        assert_eq!(shared.dropped.get(), 1);
    }

    #[test]
    fn an_observer_never_overwrites_existing_evidence() {
        let path = std::env::temp_dir().join(format!(
            "{}-observer-test-{}",
            kumochi_protocol::constants::OBJECT_PREFIX,
            std::process::id()
        ));
        let mut file = std::fs::File::create_new(&path).unwrap();
        std::io::Write::write_all(&mut file, b"old evidence").unwrap();
        drop(file);
        let result = Observer::prepare(Some(&path));
        let kept = std::fs::read(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(kept, b"old evidence");
    }
}
