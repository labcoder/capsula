//! Activation-time motion: the shared spring, scale and entrance math, the synthetic word
//! schedule, and an allocation-free sampler over activated layout.
use crate::{
    Handle, Ms, Op, Text,
    deferred::Deferred,
    display::{Item, Paint, Prim},
    geom::{Affine, Vec2},
    layout::{LayoutState, PrepareError, PreparedStep, Rest},
    limits::{MAX_LIVE_OBJECTS, MAX_MOTION_MS, MAX_OBJECT_DEPTH, MAX_SOLIDS},
    record::{Anim, Ease, Enter, KeyValue, Rgb, Track},
    state::{object, work},
    theme::Theme,
    three::{Transform, view::ViewGeometry},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};
pub use three::ViewFrame;
use three::ViewMotion;

mod frame;
mod three;
/// K̃ = (1 + K)/S(1) − 1 = 0.375891 rounded up: the normalised spring's largest overshoot.
pub(crate) const SPRING_OVERSHOOT: f32 = 0.3759;
const SPRING_DECAY: f64 = 6.0;
const SPRING_DAMPED: f64 = 19.078_784_028_338_912;
const SPRING_END: f64 = 0.997_408_957_088_984_8;
const POP_FROM: f32 = 0.82;
const POP_SPAN: f32 = 0.18;
const GROW_FROM: f32 = 0.1;
const SLIDE_OFFSET: f32 = 14.0;
const FLY_OFFSET: f32 = 48.0;
const GLIDE_MS: u32 = 450;
const MIN_SYNTHETIC_MS: u64 = 800;

#[expect(
    clippy::cast_possible_truncation,
    clippy::as_conversions,
    reason = "sampled values stay inside admitted keys and their analytical envelopes"
)]
fn narrow(value: f64) -> f32 {
    value as f32
}

/// The ζ = 0.3 step normalised by S(1), so it ends at exactly 1 with no snap.
fn spring(t: f64) -> f64 {
    if t >= 1.0 {
        return 1.0;
    }
    let t = t.max(0.0);
    let (sin, cos) = (SPRING_DAMPED * t).sin_cos();
    (1.0 - (-SPRING_DECAY * t).exp() * (cos + SPRING_DECAY / SPRING_DAMPED * sin)) / SPRING_END
}

fn ease(kind: Ease, t: f32) -> f32 {
    let t = f64::from(t.clamp(0.0, 1.0));
    narrow(match kind {
        Ease::Linear => t,
        Ease::In => t * t * t,
        Ease::Out => 1.0 - (1.0 - t).powi(3),
        Ease::InOut if t < 0.5 => 4.0 * t * t * t,
        Ease::InOut => 1.0 - (2.0 - 2.0 * t).powi(3) / 2.0,
        Ease::Spring => spring(t),
    })
}

/// Geometric interpolation between positive scale keys; spring progress may pass 1.
pub(crate) fn interpolate_scale(from: f32, to: f32, progress: f32) -> f32 {
    let (from, to) = (f64::from(from).ln(), f64::from(to).ln());
    narrow((from + f64::from(progress) * (to - from)).exp())
}

/// Admission's `(lo, hi, offset)` for an entrance, bounding exactly what the sampler draws.
pub(crate) fn entrance_envelope(enter: Enter) -> (f32, f32, f32) {
    match enter {
        Enter::None | Enter::Fade | Enter::Draw => (1.0, 1.0, 0.0),
        Enter::Pop => (
            POP_FROM,
            POP_FROM + POP_SPAN * (1.0 + SPRING_OVERSHOOT),
            0.0,
        ),
        Enter::Slide => (1.0, 1.0, SLIDE_OFFSET),
        Enter::Grow => (GROW_FROM, 1.0, 0.0),
        Enter::Fly => (1.0, 1.0, FLY_OFFSET),
    }
}

#[derive(Debug, Clone, Copy)]
struct Effect {
    alpha: f32,
    scale: f32,
    offset: f32,
    draw: f32,
}
fn effect(enter: Enter, progress: f32) -> Effect {
    let out = ease(Ease::Out, progress);
    let mut effect = Effect {
        alpha: 1.0,
        scale: 1.0,
        offset: 0.0,
        draw: 1.0,
    };
    match enter {
        Enter::None => {}
        Enter::Fade => effect.alpha = progress,
        Enter::Draw => effect.draw = out,
        Enter::Pop => {
            effect.alpha = (progress * 2.5).min(1.0);
            effect.scale = POP_FROM + POP_SPAN * ease(Ease::Spring, progress);
        }
        Enter::Slide => {
            effect.alpha = out;
            effect.offset = SLIDE_OFFSET * (1.0 - out);
        }
        Enter::Grow => {
            effect.alpha = progress;
            effect.scale = GROW_FROM + (1.0 - GROW_FROM) * out;
        }
        Enter::Fly => {
            effect.alpha = (progress * 3.0).min(1.0);
            effect.offset = FLY_OFFSET * (1.0 - out);
        }
    }
    effect
}

fn period(anim: &Anim) -> u32 {
    anim.keys.last().map_or(0, |key| key.0.get())
}
fn plays(anim: &Anim) -> u32 {
    let period = period(anim);
    if period == 0 {
        return 1;
    }
    u32::try_from(anim.repeat).map_or_else(
        |_| (MAX_MOTION_MS.saturating_sub(anim.delay.map_or(0, Ms::get)) / period).max(1),
        |repeat| repeat.saturating_add(1),
    )
}
/// The key a finished track rests on: its last key, or its first after an even yoyo.
pub(crate) fn final_key(anim: &Anim) -> &KeyValue {
    let index = if anim.yoyo && plays(anim).is_multiple_of(2) {
        0
    } else {
        anim.keys.len().saturating_sub(1)
    };
    &anim.keys[index].1
}

/// Synthetic narration length until speech timing exists: 2.75 words per second, at least
/// 800 ms, and never capped, so long narration is not truncated.
pub fn synthetic_duration_ms(say: Option<&Text>) -> u64 {
    let words = say.map_or(0, |say| word_count(say.as_str()));
    u64::try_from(words)
        .map_or(u64::MAX, |words| words.saturating_mul(4000).div_ceil(11))
        .max(MIN_SYNTHETIC_MS)
}

/// Offset of word `word` within a narration segment of `segment_ms`, proportional to its index.
pub fn at_word_offset_ms(say: &str, word: u16, segment_ms: u64) -> u64 {
    word_offset_ms(word_count(say), word, segment_ms)
}
fn word_count(say: &str) -> usize {
    say.split_whitespace().count()
}
fn word_offset_ms(words: usize, word: u16, segment_ms: u64) -> u64 {
    let words = u128::try_from(words).unwrap_or(u128::MAX);
    let offset = u128::from(segment_ms) * u128::from(word).min(words) / words.max(1);
    u64::try_from(offset).unwrap_or(u64::MAX)
}

fn at_word(op: &Op) -> Option<u16> {
    match op {
        Op::Line(r) | Op::Arrow(r) => r.at_word,
        Op::Path(r) => r.at_word,
        Op::Shape(r) => r.at_word,
        Op::Text(r) => r.at_word,
        Op::Callout(r) => r.at_word,
        Op::Highlight(r) => r.at_word,
        Op::Diagram(r) => r.at_word,
        Op::Node(r) => r.at_word,
        Op::Edge(r) => r.at_word,
        Op::Table(r) => r.at_word,
        Op::Row(r) => r.at_word,
        Op::Chart(r) => r.at_word,
        Op::Series(r) => r.at_word,
        Op::Anim(r) => r.at_word,
        Op::Mermaid(_) | Op::View3d(_) | Op::Solid(_) => None,
    }
}

/// S1's per-kind entrance length, used when an entrance names no `dur`.
fn default_duration(op: &Op) -> u32 {
    match op {
        Op::Line(_) | Op::Arrow(_) | Op::Path(_) => 600,
        Op::Highlight(_) => 450,
        Op::Edge(_) => 500,
        Op::Text(text) => u32::try_from(text.text.as_str().chars().count()).map_or(1200, |chars| {
            (chars.saturating_mul(1000) / 45).clamp(250, 1200)
        }),
        Op::Shape(_) | Op::Node(_) | Op::Callout(_) | Op::Row(_) => 350,
        Op::Series(_) => 800,
        Op::View3d(_) | Op::Solid(_) => 0,
        _ => 250,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Value {
    Num(f32),
    V2(Vec2),
    Color([f32; 3]),
    V3([f32; 3]),
}
fn channels(color: Rgb) -> [f32; 3] {
    let [_, red, green, blue] = color.get().to_be_bytes();
    [red, green, blue].map(|channel| f32::from(channel) / 255.0)
}
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::as_conversions,
    reason = "clamped unit channels round into one byte"
)]
fn rgb(channels: [f32; 3]) -> Rgb {
    let [red, green, blue] =
        channels.map(|channel| (channel.clamp(0.0, 1.0) * 255.0).round() as u8);
    Rgb::try_from(u32::from_be_bytes([0, red, green, blue])).expect("three bytes fit 24-bit RGB")
}
fn compile(value: &KeyValue, theme: &Theme) -> Value {
    match value {
        KeyValue::Num(value) => Value::Num(value.get()),
        KeyValue::V2(value) => Value::V2(Vec2::new(value[0].get(), value[1].get())),
        KeyValue::Color(color) => Value::Color(channels(theme.resolve(color))),
        KeyValue::V3(value) => Value::V3(value.map(crate::Finite::get)),
    }
}
fn interpolate(track: Track, from: Value, to: Value, progress: f32) -> Value {
    let lerp = |from: f32, to: f32| {
        narrow(f64::from(from) + f64::from(progress) * (f64::from(to) - f64::from(from)))
    };
    match (from, to) {
        (Value::Num(from), Value::Num(to)) if track == Track::Scale => {
            Value::Num(interpolate_scale(from, to, progress))
        }
        (Value::Num(from), Value::Num(to)) => Value::Num(lerp(from, to)),
        (Value::V2(from), Value::V2(to)) => {
            Value::V2(Vec2::new(lerp(from.x, to.x), lerp(from.y, to.y)))
        }
        (Value::Color(from), Value::Color(to)) => Value::Color([
            lerp(from[0], to[0]),
            lerp(from[1], to[1]),
            lerp(from[2], to[2]),
        ]),
        (Value::V3(from), Value::V3(to)) => {
            Value::V3(std::array::from_fn(|i| lerp(from[i], to[i])))
        }
        (from, _) => from,
    }
}
fn fraction(part: u64, whole: u64) -> f32 {
    match (u32::try_from(part), u32::try_from(whole)) {
        (Ok(part), Ok(whole)) if whole > 0 => narrow(f64::from(part) / f64::from(whole)),
        _ => 1.0,
    }
}

/// One activated explicit track or generated glide, compiled once at activation. Its keys are
/// immutable, so every staged view that carries the track shares them.
#[derive(Debug, Clone)]
struct Live {
    target: Handle,
    track: Track,
    origin: u64,
    keys: Arc<[(u32, Value)]>,
    ease: Ease,
    plays: u32,
    yoyo: bool,
    /// Started by the step this view activated, so [`SceneView::delay_entrance`] moves it.
    fresh: bool,
}
impl Live {
    fn new(anim: &Anim, target: Handle, origin: u64, theme: &Theme) -> Self {
        Self {
            target,
            track: anim.track,
            origin,
            keys: anim
                .keys
                .iter()
                .map(|(time, value)| (time.get(), compile(value, theme)))
                .collect(),
            ease: anim.ease,
            plays: plays(anim),
            yoyo: anim.yoyo,
            fresh: true,
        }
    }
    /// Requested bytes of this holder's key array allocation: the Arc header and the keys,
    /// padded to the allocation's alignment as the allocator is asked for it.
    fn key_bytes(&self) -> usize {
        (crate::layout::heap::ARC_HEADER + size_of_val(&*self.keys))
            .next_multiple_of(align_of::<usize>().max(align_of::<(u32, Value)>()))
    }
    fn period(&self) -> u32 {
        self.keys.last().map_or(0, |key| key.0)
    }
    /// The first instant values change; before it the track holds its first key.
    fn start(&self) -> u64 {
        self.origin
            .saturating_add(u64::from(self.keys.first().map_or(0, |key| key.0)))
    }
    fn end(&self) -> u64 {
        self.origin
            .saturating_add(u64::from(self.period()) * u64::from(self.plays))
    }
    fn last(&self) -> Value {
        let index = if self.yoyo && self.plays.is_multiple_of(2) {
            0
        } else {
            self.keys.len() - 1
        };
        self.keys[index].1
    }
    fn sample(&self, now: u64) -> Value {
        let first = self.keys[0].1;
        let period = u64::from(self.period());
        let Some(local) = now.checked_sub(self.origin) else {
            return first;
        };
        if period == 0 {
            return first;
        }
        if local >= period * u64::from(self.plays) {
            return self.last();
        }
        let mut time = local % period;
        if self.yoyo && (local / period) % 2 == 1 {
            time = period - time;
        }
        let time = u32::try_from(time).expect("lap time is within one admitted period");
        let next = self.keys.partition_point(|key| key.0 < time);
        if next == 0 {
            return first;
        }
        let (start, from) = self.keys[next - 1];
        let (end, to) = self.keys[next];
        let progress = narrow(f64::from(time - start) / f64::from(end - start));
        interpolate(self.track, from, to, ease(self.ease, progress))
    }
}

/// When an object appears and how long its entrance runs; `flow` is a dash period in ms.
#[derive(Debug, Clone, Copy)]
struct Timing {
    start: u64,
    end: u64,
    enter: Enter,
    flow: u32,
}
const DONE: Timing = Timing {
    start: 0,
    end: 0,
    enter: Enter::None,
    flow: 0,
};

#[derive(Debug)]
struct Node {
    handle: Handle,
    parent: Option<usize>,
    primitives: Arc<[Prim]>,
    label_start: usize,
    transform: Affine,
    pivot: Vec2,
    rest: Rest,
    opacity: f32,
    inherited: f32,
    timing: Timing,
    /// Put by the step this view activated, so [`SceneView::delay_entrance`] moves its timing.
    fresh: bool,
    owners: [Option<usize>; 10],
}

fn slot(track: Track) -> usize {
    match track {
        Track::X => 0,
        Track::Y => 1,
        Track::Pos => 2,
        Track::Opacity => 3,
        Track::Scale => 4,
        Track::Rotate => 5,
        Track::Draw => 6,
        Track::Color => 7,
        Track::Pos3 => 8,
        Track::Rot3 => 9,
    }
}

/// Orders placed objects parent-first, so one pass composes every world transform.
fn build_nodes(layout: &LayoutState, timing: impl Fn(&Item) -> Timing) -> Vec<Node> {
    let items: BTreeMap<Handle, (&Item, Rest, f32, usize)> = layout
        .placed()
        .map(|entry| (entry.0.handle, entry))
        .collect();
    let depth = |mut handle: Handle| {
        let mut depth = 0;
        while depth < MAX_OBJECT_DEPTH
            && let Some(parent) = items.get(&handle).and_then(|(item, ..)| item.parent)
        {
            depth += 1;
            handle = parent;
        }
        depth
    };
    let mut order: Vec<(usize, Handle)> = items
        .keys()
        .map(|handle| (depth(*handle), *handle))
        .collect();
    order.sort_unstable();
    let index: BTreeMap<Handle, usize> = order
        .iter()
        .enumerate()
        .map(|(index, (_, handle))| (*handle, index))
        .collect();
    order
        .iter()
        .map(|(_, handle)| {
            let (item, rest, inherited, label_start) = items[handle];
            Node {
                handle: *handle,
                parent: item.parent.and_then(|parent| index.get(&parent).copied()),
                primitives: Arc::clone(&item.primitives),
                label_start,
                transform: item.transform,
                pivot: item.bounds.center(),
                rest,
                opacity: item.opacity,
                inherited,
                timing: timing(item),
                fresh: false,
                owners: [None; 10],
            }
        })
        .collect()
}

/// One object as painted at a sampled instant.
#[derive(Clone, PartialEq)]
pub struct FrameItem {
    /// Object identity.
    pub handle: Handle,
    /// Prepared geometry, shared with the layout rather than copied.
    pub primitives: Arc<[Prim]>,
    label_start: usize,
    /// World transform, including ancestors, pose and entrance.
    pub transform: Affine,
    /// World opacity: ancestors, inherited factor, own opacity and entrance.
    pub opacity: f32,
    /// Stroke reveal fraction for draw tracks and entrances.
    pub draw: f32,
    /// Color override from a color track or its resting value; none keeps prepared paint.
    pub color: Option<Paint>,
    /// Dash phase of a flowing edge, in periods.
    pub flow: f32,
}
impl fmt::Debug for FrameItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameItem")
            .field("primitives", &self.primitives.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy)]
struct World {
    transform: Affine,
    opacity: f32,
    draw: f32,
    color: Option<Paint>,
    flow: f32,
    visible: bool,
}
const HIDDEN: World = World {
    transform: Affine([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]),
    opacity: 0.0,
    draw: 0.0,
    color: None,
    flow: 0.0,
    visible: false,
};

/// Reusable frame output, reserved for a full scene once so sampling never allocates.
///
/// A frame binds to the view generation it first samples after it was created or cleared.
/// Its items share geometry with that generation's objects, so [`SceneView::frame`] never
/// drops the last reference to any primitives and never frees. [`SceneView::install`], and so
/// [`SceneView::activate`], clears the frame it is handed. After [`SceneView::new`], or an
/// activation handed another frame, call [`Frame::clear`] outside paint before sampling
/// again, and clear it when its scene clears so it keeps no geometry resident past the scene.
pub struct Frame {
    items: Vec<FrameItem>,
    world: Vec<World>,
    /// The scene's sampled view; its pose storage moves to `poses` when cleared.
    view: Option<ViewFrame>,
    /// Pose storage while no view is bound, reserved for a full view once.
    poses: Vec<Transform>,
    generation: Option<Arc<()>>,
}
impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frame")
            .field("items", &self.items.len())
            .finish_non_exhaustive()
    }
}
impl Default for Frame {
    fn default() -> Self {
        Self::new()
    }
}
impl Frame {
    /// Reserves room for the per-scene object cap.
    pub fn new() -> Self {
        Self {
            items: Vec::with_capacity(MAX_LIVE_OBJECTS),
            world: Vec::with_capacity(MAX_LIVE_OBJECTS),
            view: None,
            poses: Vec::with_capacity(MAX_SOLIDS),
            generation: None,
        }
    }
    /// Objects in handle order. Use [`Self::paint_items`] for layered painting.
    pub fn items(&self) -> &[FrameItem] {
        &self.items
    }
    /// The sampled 3D view, if the scene has one: at most `MAX_VIEWS`.
    pub fn views(&self) -> &[ViewFrame] {
        self.view.as_slice()
    }
    /// Ordinary geometry followed by edge-label chips and text, without allocation, each with
    /// the index of its first primitive in [`FrameItem::primitives`].
    pub fn paint_items(&self) -> impl Iterator<Item = (&FrameItem, usize, &[Prim])> {
        let labels = self.items.iter().map(|item| {
            let start = item.label_start;
            (item, start, &item.primitives[start..])
        });
        self.items
            .iter()
            .map(|item| (item, 0, &item.primitives[..item.label_start]))
            .chain(labels)
            .filter(|(_, _, primitives)| !primitives.is_empty())
    }
    /// The view generation this frame is bound to, once it has sampled one.
    pub fn generation(&self) -> Option<&Arc<()>> {
        self.generation.as_ref()
    }
    /// Releases shared geometry and the view binding while keeping the reserved capacity.
    /// It may free the last references to replaced primitives, so call it outside paint.
    pub fn clear(&mut self) {
        self.items.clear();
        self.world.clear();
        if let Some(ViewFrame { poses, .. }) = self.view.take() {
            self.poses = poses;
            self.poses.clear();
        }
        self.generation = None;
    }
    /// Requested bytes of the reserved item, world and pose storage; items and the view share
    /// their geometry.
    pub fn resident_bytes(&self) -> usize {
        let poses =
            self.poses.capacity() + self.view.as_ref().map_or(0, |view| view.poses.capacity());
        self.items.capacity() * size_of::<FrameItem>()
            + self.world.capacity() * size_of::<World>()
            + poses * size_of::<Transform>()
    }
}

/// The activated picture of one scene: layout at rest plus the motion it still owes.
///
/// The host supplies a monotonic clock in milliseconds; the view never reads one.
pub struct SceneView {
    layout: LayoutState,
    /// The layout's full requested bytes, geometry included, cached so retirement never walks it.
    layout_bytes: usize,
    activated: u32,
    deadline: u64,
    nodes: Vec<Node>,
    paint: Vec<usize>,
    tracks: Vec<Live>,
    /// The scene's 3D view, if any.
    view: Option<ViewMotion>,
    generation: Arc<()>,
}
impl fmt::Debug for SceneView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SceneView")
            .field("activated", &self.activated)
            .field("objects", &self.nodes.len())
            .field("tracks", &self.tracks.len())
            .finish_non_exhaustive()
    }
}

/// One step's activation, staged from a borrowed view and step without changing either: the
/// candidate view, the candidates newer owners superseded, and the generation of the view it
/// was staged from, which alone may install it. Dropped uninstalled, as a refused host
/// transition drops it, it goes whole to the dropper.
pub struct PreparedActivation(Deferred<(SceneView, Vec<Option<Live>>, Arc<()>)>);
impl PreparedActivation {
    /// The candidate view; installing it keeps its generation.
    pub fn view(&self) -> &SceneView {
        let (view, ..) = &*self.0;
        view
    }
    /// Requested bytes the installed view owns: its [`SceneView::resident_bytes`].
    pub fn resident_bytes(&self) -> usize {
        let (view, ..) = &*self.0;
        view.resident_bytes()
    }
}
impl fmt::Debug for PreparedActivation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (view, superseded, _) = &*self.0;
        f.debug_struct("PreparedActivation")
            .field("view", view)
            .field("superseded", &superseded.len())
            .finish()
    }
}

impl SceneView {
    /// Installs a layout at rest with no live motion: recovery and every post-cut tip.
    /// Pass `scene.activated()`, never the layout's revision, which counts dropped commits:
    /// `SceneView::new(&view.layout().cut(&scene)?, scene.activated())`.
    ///
    /// The view starts a new generation, so a [`Frame`] bound to an earlier view must be
    /// cleared outside paint before [`SceneView::frame`] accepts it.
    pub fn new(at_rest: &LayoutState, activated: u32) -> Self {
        let nodes = build_nodes(at_rest, |_| DONE);
        Self {
            paint: paint_order(&nodes),
            nodes,
            layout: at_rest.clone(),
            layout_bytes: at_rest.resident_bytes(),
            activated,
            deadline: 0,
            tracks: Vec::new(),
            view: view(at_rest).map(|geometry| ViewMotion::new(geometry, None, None)),
            generation: Arc::new(()),
        }
    }
    /// The revision a host `StopCut` must name.
    pub fn activated(&self) -> u32 {
        self.activated
    }
    /// This view's generation: a frame that samples it binds to it, and paint state staged from
    /// it holds it.
    pub fn generation(&self) -> &Arc<()> {
        &self.generation
    }
    /// Every object's primitives, shared with the frames sampled from this generation.
    pub fn paint_sources(&self) -> impl Iterator<Item = (Handle, &[Prim])> {
        self.nodes
            .iter()
            .map(|node| (node.handle, &node.primitives[..]))
    }
    /// The activated layout, which a cut freezes.
    pub fn layout(&self) -> &LayoutState {
        &self.layout
    }
    /// Requested bytes this view owns beyond geometry its layout shares: the layout's own
    /// index copy, nodes, paint order, tracks with their key arrays, and the generation. A key
    /// array a staged or outgoing view also holds is counted for each holder.
    pub fn resident_bytes(&self) -> usize {
        self.layout.index_bytes()
            + self.nodes.capacity() * size_of::<Node>()
            + self.paint.capacity() * size_of::<usize>()
            + self.tracks.capacity() * size_of::<Live>()
            + self.tracks.iter().map(Live::key_bytes).sum::<usize>()
            + self.view.as_ref().map_or(0, ViewMotion::bytes)
            + crate::layout::heap::ARC_HEADER
    }

    /// Activates a prepared step at `now`: [`Self::prepare_activation`], then
    /// [`Self::install`] into the host's paint frame `out`. The step goes to the dropper.
    /// # Errors
    /// Returns `Stale`, leaving the view and `out` untouched, unless the step was prepared
    /// on this view's layout.
    pub fn activate(
        &mut self,
        step: PreparedStep,
        now: u64,
        segment_ms: u64,
        out: &mut Frame,
    ) -> Result<(), PrepareError> {
        let installed = self
            .prepare_activation(&step, now, segment_ms)
            .and_then(|activation| self.install(activation, out));
        let bytes = step.owner_bytes();
        drop(Deferred::new(step, bytes));
        installed
    }

    /// Stages `step` at `now` without changing this view or the step, scheduling its objects
    /// and tracks against the narration segment; a later step takes over a property only
    /// from this instant. Running and delayed tracks carry over, sharing their keys.
    /// # Errors
    /// Returns `Stale` unless the step was prepared on this view's layout.
    pub fn prepare_activation(
        &self,
        step: &PreparedStep,
        now: u64,
        segment_ms: u64,
    ) -> Result<PreparedActivation, PrepareError> {
        if !step.is_based_on(&self.layout) {
            return Err(PrepareError::Stale);
        }
        let source = step.run();
        let say = source.say.as_ref().map_or("", Text::as_str);
        let words = word_count(say);
        let origin = |word: Option<u16>, delay: Option<Ms>| {
            now.saturating_add(word_offset_ms(words, word.unwrap_or(0), segment_ms))
                .saturating_add(u64::from(delay.map_or(0, Ms::get)))
        };
        let born = births(step, origin);
        let recolored: BTreeSet<Handle> = source
            .set
            .iter()
            .filter(|change| change.color.is_some())
            .filter_map(|change| step.after.handle(&change.target))
            .collect();
        let glides: Vec<Live> = step
            .glides
            .iter()
            .map(|glide| Live {
                target: glide.target,
                track: Track::Pos,
                origin: now,
                keys: Arc::new([
                    (
                        0,
                        Value::V2(self.position(glide.target, now).unwrap_or(glide.to)),
                    ),
                    (GLIDE_MS, Value::V2(glide.to)),
                ]),
                ease: Ease::InOut,
                plays: 1,
                yoyo: false,
                fresh: true,
            })
            .collect();
        let mut candidates = self.carried(now, &recolored);
        candidates.extend(glides);
        // shortcut: explicit tracks keep the step clock under delayed parents; revisit when model instructions need parent-relative timing.
        for op in &*source.put {
            if let Op::Anim(anim) = op
                && let Some(target) = step
                    .after
                    .handle(&anim.target)
                    .or_else(|| step.after.is_three(&anim.target))
            {
                let start = origin(anim.at_word, anim.delay);
                candidates.push(Live::new(anim, target, start, step.theme()));
            }
        }
        let deadline = self.deadline;
        let previous: BTreeMap<Handle, Timing> = self
            .nodes
            .iter()
            .map(|node| {
                let timing = node.timing;
                let settled = deadline <= now || (timing.flow == 0 && timing.end <= now);
                (node.handle, if settled { DONE } else { timing })
            })
            .collect();
        let mut nodes = build_nodes(&step.after, |item| {
            born.get(&item.handle)
                .or_else(|| previous.get(&item.handle))
                .copied()
                .unwrap_or(DONE)
        });
        let mut starts: Vec<u64> = Vec::with_capacity(nodes.len());
        for node in &mut nodes {
            node.fresh = born.contains_key(&node.handle);
            if let Some(parent) = node.parent
                && node.fresh
            {
                let wait = starts[parent].saturating_sub(node.timing.start);
                node.timing.start = node.timing.start.saturating_add(wait);
                node.timing.end = node.timing.end.saturating_add(wait);
            }
            // A surviving node already holds its visible start: ids cannot be re-put and parents cannot change.
            starts.push(node.timing.start);
        }
        let mut view = view(&step.after)
            .map(|geometry| ViewMotion::new(geometry, self.view.as_ref(), Some(now)));
        let (tracks, superseded) = own(&mut nodes, view.as_mut(), candidates);
        let view = Self {
            paint: paint_order(&nodes),
            nodes,
            layout: step.after.clone(),
            layout_bytes: step.retained_bytes(),
            activated: step.index + 1,
            deadline: now.saturating_add(u64::from(MAX_MOTION_MS)),
            tracks,
            view,
            generation: Arc::new(()),
        };
        let bytes =
            view.attributed_bytes() + retired_bytes(&superseded) + crate::layout::heap::ARC_HEADER;
        let source = Arc::clone(&self.generation);
        Ok(PreparedActivation(Deferred::new(
            (view, superseded, source),
            bytes,
        )))
    }

    /// Installs an activation staged from this view's current generation. `out` is the host's
    /// paint frame: it is cleared first, while the outgoing objects still own its geometry, and
    /// binds to the new generation on its next [`SceneView::frame`]. Any other frame bound to
    /// this view must then be cleared outside paint before it is sampled again.
    /// # Errors
    /// Returns `Stale`, leaving the view and `out` untouched, when the activation was staged
    /// from another view or from this one before it changed; the activation goes to the dropper.
    pub fn install(
        &mut self,
        activation: PreparedActivation,
        out: &mut Frame,
    ) -> Result<(), PrepareError> {
        let (_, _, source) = &*activation.0;
        if !Arc::ptr_eq(source, &self.generation) {
            return Err(PrepareError::Stale);
        }
        out.clear();
        let (view, superseded, source) = activation.0.into_inner();
        // The outgoing layout may hold the last references to geometry this step removed.
        let outgoing = std::mem::replace(self, view);
        let bytes = outgoing.attributed_bytes()
            + retired_bytes(&superseded)
            + crate::layout::heap::ARC_HEADER;
        drop(Deferred::new((outgoing, superseded, source), bytes));
        Ok(())
    }

    /// Starts what this view's step put `by` ms later than it was staged: its objects' appearances
    /// and entrances, its tracks and glides, a newly put view's spin and the motion deadline. A
    /// host that painted the activation late begins it there. Carried tracks and earlier objects
    /// keep their times.
    pub fn delay_entrance(&mut self, by: u64) {
        for node in self.nodes.iter_mut().filter(|node| node.fresh) {
            node.timing.start = node.timing.start.saturating_add(by);
            node.timing.end = node.timing.end.saturating_add(by);
        }
        for live in self.tracks.iter_mut().filter(|live| live.fresh) {
            live.origin = live.origin.saturating_add(by);
        }
        if let Some(view) = &mut self.view {
            view.delay(by);
        }
        self.deadline = self.deadline.saturating_add(by);
    }

    /// The earliest instant an object this view's step put appears with something to paint,
    /// clipped to the deadline, or `None` if it put none; carried objects don't count. A host
    /// whose first sample of the step had nothing to paint samples it no later than this, so a
    /// late wake can't take the object's first picture part-way into its entrance.
    pub fn first_appearance(&self) -> Option<u64> {
        self.paint
            .iter()
            .map(|&index| &self.nodes[index])
            .filter(|node| node.fresh)
            .map(|node| node.timing.start.min(self.deadline))
            .min()
    }

    /// What retiring this view may free: its own bytes plus its layout's cached geometry, which
    /// it may hold last. Counted per holder, from the cache, so retirement walks nothing.
    fn attributed_bytes(&self) -> usize {
        self.resident_bytes() + self.layout_bytes.saturating_sub(self.layout.index_bytes())
    }

    /// Running and delayed tracks that survive into the next activation, sharing their keys.
    /// Settled and recolored tracks rest in layout and retire with the outgoing view.
    fn carried(&self, now: u64, recolored: &BTreeSet<Handle>) -> Vec<Live> {
        let deadline = self.deadline;
        self.tracks
            .iter()
            .filter(|live| {
                live.end().min(deadline) > now
                    && (live.track != Track::Color || !recolored.contains(&live.target))
            })
            .map(|live| Live {
                fresh: false,
                ..live.clone()
            })
            .collect()
    }

    /// The live position of a glide target, sampled before the new step installs.
    fn position(&self, handle: Handle, now: u64) -> Option<Vec2> {
        let node = self.nodes.iter().find(|node| node.handle == handle)?;
        let owner = node.owners[slot(Track::Pos)].filter(|_| now < self.deadline);
        Some(match owner.map(|owner| self.tracks[owner].sample(now)) {
            Some(Value::V2(position)) => position,
            _ => node.rest.pos,
        })
    }

    /// Whether any motion changes the picture at `now`. A hold (a delay, or a first key
    /// after zero) is not motion; the host then sleeps until `next_wake`.
    pub fn animating(&self, now: u64) -> bool {
        if now >= self.deadline {
            return false;
        }
        let active = |start: u64, end: u64| start <= now && now < end;
        self.nodes.iter().any(|node| {
            let timing = node.timing;
            (timing.enter != Enter::None && active(timing.start, timing.end))
                || (timing.flow != 0 && timing.start <= now)
        }) || self
            .tracks
            .iter()
            .any(|live| active(live.start(), live.end()))
            || self.view_motion(now)
    }

    /// Whether a spin or a 3D track is running at `now`. A track still to start is a hold.
    pub fn view_motion(&self, now: u64) -> bool {
        let deadline = self.deadline;
        now < deadline
            && (self
                .view
                .as_ref()
                .and_then(|view| view.spin)
                .is_some_and(|spin| spin.running(now, deadline))
                || self.tracks.iter().any(|live| {
                    three::slot(live.track).is_some() && live.start() <= now && now < live.end()
                }))
    }

    /// Whether nothing still runs or waits to start at `now`, 2D or 3D: no track ending later,
    /// no object whose timing still runs or flows, and no spin. A delayed owner keeps it false.
    pub fn at_rest(&self, now: u64) -> bool {
        let deadline = self.deadline;
        now >= deadline
            || !(self.tracks.iter().any(|live| live.end() > now)
                || self
                    .nodes
                    .iter()
                    .any(|node| node.timing.end > now || node.timing.flow != 0)
                || self
                    .view
                    .as_ref()
                    .and_then(|view| view.spin)
                    .is_some_and(|spin| spin.stop(deadline) > now))
    }

    /// The Stop cut path: keeps the yaw `from` showed at scene time `now` (the sample a Stop
    /// latched), as a still angle, when this view continues the same view; a replacement view
    /// keeps its authored camera.
    pub fn carry_spin(&mut self, from: &SceneView, now: u64) {
        if let (Some(view), Some(old)) = (self.view.as_mut(), from.view.as_ref())
            && view.geometry.handle == old.geometry.handle
        {
            view.yaw = old.degrees(now, from.deadline);
        }
    }

    /// Stops the spin at its angle at `now`, as a drag does; later holds change nothing.
    pub fn hold_spin(&mut self, now: u64) {
        if let Some(spin) = self.view.as_mut().and_then(|view| view.spin.as_mut()) {
            spin.hold(now);
        }
    }

    /// The next instant after `now` when motion starts, ends or an object appears; the
    /// scene deadline counts only while some motion would still run at it.
    pub fn next_wake(&self, now: u64) -> Option<u64> {
        let deadline = self.deadline;
        let nodes = self.nodes.iter().flat_map(|node| {
            let timing = node.timing;
            [
                Some(timing.start),
                (timing.enter != Enter::None).then_some(timing.end),
                (timing.flow != 0).then_some(deadline),
            ]
        });
        let tracks = self
            .tracks
            .iter()
            .flat_map(|live| [Some(live.start()), Some(live.end())]);
        let spin = self
            .view
            .as_ref()
            .and_then(|view| view.spin)
            .map(|spin| spin.stop(deadline));
        nodes
            .chain(tracks)
            .chain([spin])
            .flatten()
            .map(|time| time.min(deadline))
            .filter(|time| *time > now)
            .min()
    }

    /// Handles whose own entrance and every ancestor's entrance finished by `now`, each
    /// clipped at the scene deadline; objects scheduled later are not revealed.
    // shortcut: only entrances gate a reveal, so Esc during an explicit opacity fade-in with no
    // `enter` shows its final opacity; fix once the recovery log records activation times.
    pub fn stop(&self, now: u64) -> Vec<Handle> {
        let mut finished = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            let done = node.timing.end.min(self.deadline) <= now
                && node.parent.is_none_or(|parent| finished[parent]);
            finished.push(done);
        }
        let mut handles: Vec<Handle> = self
            .nodes
            .iter()
            .zip(finished)
            .filter_map(|(node, done)| done.then_some(node.handle))
            .chain(self.layout.three_handles())
            .collect();
        handles.sort_unstable();
        handles
    }
}

/// Requested bytes of the superseded candidates an activation retires.
fn retired_bytes(superseded: &Vec<Option<Live>>) -> usize {
    superseded.capacity() * size_of::<Option<Live>>()
        + superseded
            .iter()
            .flatten()
            .map(Live::key_bytes)
            .sum::<usize>()
}

/// Indices of drawable nodes in handle order, the paint order of prepared display lists.
fn paint_order(nodes: &[Node]) -> Vec<usize> {
    let mut paint: Vec<usize> = (0..nodes.len())
        .filter(|index| !nodes[*index].primitives.is_empty())
        .collect();
    paint.sort_unstable_by_key(|index| nodes[*index].handle);
    paint
}

/// Schedules every object this step puts: appear at its origin, then run its entrance.
fn births(
    step: &PreparedStep,
    origin: impl Fn(Option<u16>, Option<Ms>) -> u64,
) -> BTreeMap<Handle, Timing> {
    let mut born = BTreeMap::new();
    for op in &*step.run().put {
        if let Some(visual) = object::visual(op)
            && let Some(handle) = step.after.handle(visual.id)
        {
            let start = origin(at_word(op), visual.delay);
            let enter = work::entrance(op).unwrap_or(Enter::None);
            let dur = if enter == Enter::None {
                0
            } else {
                visual.dur.map_or_else(|| default_duration(op), Ms::get)
            };
            let flow = match op {
                Op::Edge(edge) => edge.flow.map_or(0, Ms::get),
                _ => 0,
            };
            born.insert(
                handle,
                Timing {
                    start,
                    end: start.saturating_add(u64::from(dur)),
                    enter,
                    flow,
                },
            );
        }
    }
    born
}

/// The scene's prepared 3D view; a scene holds at most `MAX_VIEWS`, one.
fn view(layout: &LayoutState) -> Option<&Arc<ViewGeometry>> {
    layout.views().values().next()
}

/// Gives each (object, property) its newest candidate and keeps only owning tracks, returning
/// the superseded candidates for the caller to retire. A 3D owner is aligned by handle to its
/// solid's slot in the activated view, so compaction after a removal can't move it.
fn own(
    nodes: &mut [Node],
    mut view: Option<&mut ViewMotion>,
    candidates: Vec<Live>,
) -> (Vec<Live>, Vec<Option<Live>>) {
    let index: BTreeMap<Handle, usize> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.handle, index))
        .collect();
    let solids: BTreeMap<Handle, usize> = view.as_ref().map_or_else(BTreeMap::new, |view| {
        view.geometry
            .solids
            .iter()
            .enumerate()
            .map(|(index, solid)| (solid.handle, index))
            .collect()
    });
    for (position, live) in candidates.iter().enumerate() {
        match (three::slot(live.track), view.as_deref_mut()) {
            (Some(property), Some(view)) => {
                if let Some(&solid) = solids.get(&live.target) {
                    view.owners[solid][property] = Some(position);
                }
            }
            (Some(_), None) => {}
            (None, _) => {
                if let Some(node) = index.get(&live.target) {
                    nodes[*node].owners[slot(live.track)] = Some(position);
                }
            }
        }
    }
    let mut candidates: Vec<Option<Live>> = candidates.into_iter().map(Some).collect();
    let mut tracks = Vec::new();
    for node in nodes {
        for owner in node.owners.iter_mut().flatten() {
            tracks.push(
                candidates[*owner]
                    .take()
                    .expect("a track targets one object property"),
            );
            *owner = tracks.len() - 1;
        }
    }
    for owner in view
        .into_iter()
        .flat_map(|view| view.owners.iter_mut().flatten().flatten())
    {
        tracks.push(
            candidates[*owner]
                .take()
                .expect("a track targets one solid property"),
        );
        *owner = tracks.len() - 1;
    }
    (tracks, candidates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Record, parse_record};

    #[test]
    fn the_normalised_spring_ends_at_one_and_peaks_at_its_constant() {
        assert!(
            (spring(1.0 - 1e-12) - 1.0).abs() < 1e-9,
            "S(1) normalisation leaves no endpoint snap"
        );
        let (min, max) = (0..=100_000)
            .map(|n| spring(f64::from(n) / 100_000.0))
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(min, max), value| {
                (min.min(value), max.max(value))
            });
        assert_eq!(
            min.to_bits(),
            0.0_f64.to_bits(),
            "the spring starts at zero"
        );
        assert!((max - 1.375_891).abs() < 1e-6, "peak {max}");
        assert!(
            max < f64::from(1.0 + SPRING_OVERSHOOT),
            "the stored constant bounds the curve"
        );
    }

    #[test]
    fn random_spring_tracks_stay_inside_the_admitted_envelope() {
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            u32::try_from(seed >> 40).expect("24 bits fit u32")
        };
        for round in 0..200 {
            let scale = round % 2 == 0;
            let mut time = 0;
            let keys: Vec<serde_json::Value> = (0..2 + round % 3)
                .map(|_| {
                    let unit = f64::from(next()) / 16_777_216.0;
                    let value = if scale {
                        0.05 + 7.95 * unit
                    } else {
                        2000.0 * unit - 1000.0
                    };
                    let key = serde_json::json!([time, value]);
                    time += 1 + next() % 500;
                    key
                })
                .collect();
            let track = if scale { "scale" } else { "x" };
            let record = serde_json::json!({"step":{"id":"s","put":[{"anim":{
                "target":"t","track":track,"ease":"spring","keys":keys}}]}});
            let bytes = serde_json::to_vec(&record).expect("fixture JSON");
            let Ok(Record::Step(step)) = parse_record(&bytes) else {
                panic!("typed fixture");
            };
            let Op::Anim(anim) = &step.put[0] else {
                panic!("anim fixture");
            };
            let envelope = work::motion(anim, 0, &step).expect("admitted track");
            let handle = Handle::new(1, 0).expect("fixture handle");
            let live = Live::new(anim, handle, 0, &Theme::default());
            for now in 0..=live.end() {
                let Value::Num(value) = live.sample(now) else {
                    panic!("numeric track");
                };
                let ulp = value.abs().next_up() - value.abs();
                assert!(
                    value >= envelope.min[0] - ulp && value <= envelope.max[0] + ulp,
                    "round {round} at {now}: {value} outside {envelope:?}"
                );
            }
        }
    }

    fn live(track: Track, ease: Ease, keys: &[(u32, Value)], plays: u32) -> Live {
        Live {
            target: Handle::new(1, 0).expect("fixture handle"),
            track,
            origin: 0,
            keys: keys.into(),
            ease,
            plays,
            yoyo: false,
            fresh: false,
        }
    }

    #[test]
    fn tracks_sample_exact_values_at_keys_laps_and_eases() {
        let num = |keys: &[(u32, f32)]| -> Vec<(u32, Value)> {
            keys.iter()
                .map(|&(time, value)| (time, Value::Num(value)))
                .collect()
        };
        let check = |name: &str, live: Live, samples: &[(u64, f32)]| {
            for &(now, expected) in samples {
                assert_eq!(live.sample(now), Value::Num(expected), "{name} at {now}");
            }
        };
        let unit = num(&[(0, 0.0), (100, 1.0)]);
        let ramp = num(&[(0, 0.0), (100, 10.0)]);
        check(
            "linear",
            live(Track::X, Ease::Linear, &unit, 1),
            &[(50, 0.5)],
        );
        check("in", live(Track::X, Ease::In, &unit, 1), &[(50, 0.125)]);
        check("out", live(Track::X, Ease::Out, &unit, 1), &[(50, 0.875)]);
        check(
            "in-out",
            live(Track::X, Ease::InOut, &unit, 1),
            &[(25, 0.0625), (50, 0.5), (75, 0.9375)],
        );
        let three = num(&[(0, 0.0), (100, 10.0), (300, 30.0)]);
        check(
            "three keys",
            live(Track::X, Ease::Linear, &three, 1),
            &[
                (0, 0.0),
                (50, 5.0),
                (100, 10.0),
                (200, 20.0),
                (300, 30.0),
                (400, 30.0),
            ],
        );
        let late = num(&[(100, 4.0), (200, 8.0)]);
        check(
            "late first key",
            live(Track::X, Ease::Linear, &late, 1),
            &[(0, 4.0), (100, 4.0), (150, 6.0), (200, 8.0)],
        );
        check(
            "repeat",
            live(Track::X, Ease::Linear, &ramp, 2),
            &[(25, 2.5), (100, 0.0), (125, 2.5), (200, 10.0)],
        );
        check(
            "yoyo, repeat 2",
            Live {
                yoyo: true,
                ..live(Track::X, Ease::Linear, &ramp, 3)
            },
            &[
                (25, 2.5),
                (100, 10.0),
                (125, 7.5),
                (200, 0.0),
                (225, 2.5),
                (300, 10.0),
            ],
        );
        check(
            "yoyo, two plays",
            Live {
                yoyo: true,
                ..live(Track::X, Ease::Linear, &ramp, 2)
            },
            &[(175, 2.5), (200, 0.0)],
        );
        let scale = num(&[(0, 1.0), (100, 4.0)]);
        check(
            "scale",
            live(Track::Scale, Ease::Linear, &scale, 1),
            &[(50, 2.0)],
        );
        let color = [
            (0, Value::Color([1.0, 0.0, 0.0])),
            (100, Value::Color([0.0, 0.0, 1.0])),
        ];
        let Value::Color(channels) = live(Track::Color, Ease::Linear, &color, 1).sample(50) else {
            panic!("color track");
        };
        assert_eq!(
            rgb(channels).get(),
            0x80_00_80,
            "red to blue rounds to (128, 0, 128)"
        );
    }

    #[test]
    fn entrance_effects_stay_inside_their_admission_envelopes() {
        let alphas = [
            (Enter::None, [1.0, 1.0, 1.0]),
            (Enter::Fade, [0.0, 0.5, 1.0]),
            (Enter::Draw, [1.0, 1.0, 1.0]),
            (Enter::Pop, [0.0, 1.0, 1.0]),
            (Enter::Slide, [0.0, 0.875, 1.0]),
            (Enter::Grow, [0.0, 0.5, 1.0]),
            (Enter::Fly, [0.0, 1.0, 1.0]),
        ];
        for (enter, expected) in alphas {
            let alpha = [0.0, 0.5, 1.0].map(|progress| effect(enter, progress).alpha.to_bits());
            assert_eq!(
                alpha,
                expected.map(f32::to_bits),
                "{enter:?} alpha at 0, 0.5 and 1"
            );
            let rest = effect(enter, 1.0);
            assert_eq!(
                [rest.scale, rest.offset, rest.draw].map(f32::to_bits),
                [1.0_f32, 0.0, 1.0].map(f32::to_bits),
                "{enter:?} ends exactly at rest"
            );
            let (lo, hi, offset) = entrance_envelope(enter);
            for step in 0..=1000_u16 {
                let progress = f32::from(step) / 1000.0;
                let sampled = effect(enter, progress);
                assert!(
                    lo <= sampled.scale
                        && sampled.scale <= hi
                        && (0.0..=offset).contains(&sampled.offset),
                    "{enter:?} at {progress}: {sampled:?} outside ({lo}, {hi}, {offset})"
                );
            }
        }
    }

    #[test]
    fn default_entrance_durations_follow_each_kind() {
        let cases = [
            (
                serde_json::json!({"highlight":{"id":"h","target":[0,0]}}),
                450,
            ),
            (
                serde_json::json!({"line":{"id":"l","from":[0,0],"to":[1,1]}}),
                600,
            ),
            (serde_json::json!({"path":{"id":"p0","d":"M0 0 L1 1"}}), 600),
            (serde_json::json!({"chart":{"id":"c0"}}), 250),
            (
                serde_json::json!({"arrow":{"id":"a","from":[0,0],"to":[1,1]}}),
                600,
            ),
            (
                serde_json::json!({"edge":{"id":"e","in":"d","from":"n","to":"m"}}),
                500,
            ),
            (
                serde_json::json!({"shape":{"id":"s","kind":"rect","box":[0,0,1,1]}}),
                350,
            ),
            (
                serde_json::json!({"node":{"id":"n","in":"d","label":"N"}}),
                350,
            ),
            (
                serde_json::json!({"callout":{"id":"c","target":[0,0],"text":""}}),
                350,
            ),
            (
                serde_json::json!({"row":{"id":"r","in":"t","cells":[1]}}),
                350,
            ),
            (
                serde_json::json!({"series":{"id":"p","in":"c","data":[]}}),
                800,
            ),
            (
                serde_json::json!({"text":{"id":"t0","at":[0,0],"text":""}}),
                250,
            ),
            (
                serde_json::json!({"text":{"id":"t1","at":[0,0],"text":"a".repeat(20)}}),
                444,
            ),
            (
                serde_json::json!({"text":{"id":"t2","at":[0,0],"text":"a".repeat(60)}}),
                1200,
            ),
            (serde_json::json!({"diagram":{"id":"d"}}), 250),
            (serde_json::json!({"table":{"id":"t","cols":["V"]}}), 250),
        ];
        let put: Vec<serde_json::Value> = cases.iter().map(|(op, _)| op.clone()).collect();
        let record = serde_json::json!({"step":{"id":"s","put":put}});
        let bytes = serde_json::to_vec(&record).expect("fixture JSON");
        let Ok(Record::Step(step)) = parse_record(&bytes) else {
            panic!("typed fixture");
        };
        for (op, (fixture, expected)) in step.put.iter().zip(&cases) {
            assert_eq!(default_duration(op), *expected, "{fixture}");
        }
    }
}
