use super::{PrepareError, heap, primitives};
use crate::{
    ErrorCode, Handle, Op, SceneState, WireId,
    deferred::Deferred,
    display::{Item, PaintWork},
    geom::{Affine, Rect, Vec2},
    mermaid::Extra,
    record::{Direction, KeyValue, Rgb, Track},
    theme::Theme,
    three::view::ViewGeometry,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

#[cfg(test)]
use crate::display::Prim;
#[cfg(test)]
#[path = "../../tests/support/connected_stroke.rs"]
mod connected_stroke;

/// Resting values of explicit tracks; `None` means no explicit owner, so the base applies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Rest {
    pub pos: Vec2,
    pub x: f32,
    pub y: f32,
    pub scale: f32,
    pub rotate: f32,
    pub opacity: Option<f32>,
    pub draw: Option<f32>,
    pub color: Option<Rgb>,
}
impl Default for Rest {
    fn default() -> Self {
        Self {
            pos: Vec2::default(),
            x: 0.0,
            y: 0.0,
            scale: 1.0,
            rotate: 0.0,
            opacity: None,
            draw: None,
            color: None,
        }
    }
}
impl Rest {
    pub(crate) fn transform(self) -> Affine {
        Affine::pose(
            self.scale,
            self.rotate.to_radians(),
            self.pos + Vec2::new(self.x, self.y),
        )
    }
    pub(super) fn set(&mut self, track: Track, value: &KeyValue, theme: &Theme) {
        match (track, value) {
            (Track::Pos, KeyValue::V2(v)) => self.pos = Vec2::new(v[0].get(), v[1].get()),
            (Track::X, KeyValue::Num(v)) => self.x = v.get(),
            (Track::Y, KeyValue::Num(v)) => self.y = v.get(),
            (Track::Scale, KeyValue::Num(v)) => self.scale = v.get(),
            (Track::Rotate, KeyValue::Num(v)) => self.rotate = v.get(),
            (Track::Opacity, KeyValue::Num(v)) => self.opacity = Some(v.get()),
            (Track::Draw, KeyValue::Num(v)) => self.draw = Some(v.get()),
            (Track::Color, KeyValue::Color(c)) => self.color = Some(theme.resolve(c)),
            _ => {}
        }
    }
}

/// What [`LayoutState::placed`] yields for one object.
pub(crate) type PlacedEntry<'a> = (&'a Item, Rest, f32, usize, f32, Option<Handle>);

/// Explicit motion envelopes by property, shared between prefixes until one changes them.
pub(super) type Motions = BTreeMap<&'static str, crate::state::work::Motion>;

#[derive(Debug, Clone)]
pub(super) struct Placed {
    pub item: Item,
    pub source: Arc<Op>,
    pub anchors: [Vec2; 2],
    pub edge_direction: Direction,
    pub rest: Rest,
    pub rank: Option<usize>,
    pub motion: Option<Arc<Motions>>,
    pub chart_basis: Option<Arc<super::chart::Basis>>,
    /// Resting opacity of ancestors a cut detached; `item.opacity` stays the object's own base.
    pub inherited_opacity: f32,
    /// The measured size of a group's private title, which its band reserves.
    pub title: Option<Vec2>,
    /// Paint work of `item.primitives`, recounted when they or `paint_scale` change.
    pub work: PaintWork,
    /// Composed scale `work` charged the rect fallbacks at; the painter refuses a pose whose pieces
    /// pass that charge.
    pub paint_scale: f32,
    /// The latest `set.behind` anchor. Once it is removed or cut, `LayoutState::held` keeps its
    /// place.
    pub behind: Option<Handle>,
}

impl Placed {
    pub(super) fn new(mut item: Item, source: Arc<Op>, cached: Option<&Self>) -> Self {
        if let Some(old) = cached {
            item.transform = old.item.transform;
        }
        Self {
            behind: cached.and_then(|old| old.behind),
            item,
            source,
            anchors: cached.map_or([Vec2::default(); 2], |old| old.anchors),
            edge_direction: cached.map_or(Direction::Down, |old| old.edge_direction),
            rest: cached.map_or_else(Rest::default, |old| old.rest),
            rank: cached.and_then(|old| old.rank),
            motion: cached.and_then(|old| old.motion.as_ref().map(Arc::clone)),
            chart_basis: cached.and_then(|old| old.chart_basis.as_ref().map(Arc::clone)),
            inherited_opacity: cached.map_or(1.0, |old| old.inherited_opacity),
            title: cached.and_then(|old| old.title),
            work: PaintWork::default(),
            paint_scale: 0.0,
        }
    }
}

/// Placed objects from the prepared prefix, sharing immutable geometry.
#[derive(Clone)]
pub struct LayoutState {
    pub(super) owner: Arc<()>,
    pub(super) cut_revision: u32,
    pub(super) revision: u32,
    pub(super) version: Arc<()>,
    pub(super) objects: BTreeMap<WireId, Placed>,
    pub(super) handles: BTreeMap<Handle, WireId>,
    /// Private compiled facts, shared per revision that created them.
    pub(super) extras: BTreeMap<u32, Arc<BTreeMap<usize, Extra>>>,
    /// Prepared 3D views by view id.
    pub(super) views: BTreeMap<WireId, Arc<ViewGeometry>>,
    pub(super) canvas: Rect,
    pub(super) declared_canvas: bool,
    pub(super) content: Option<Rect>,
    pub(super) presentation_scale: f32,
    pub(super) presentation_limit: f32,
    /// Removed or cut `set.behind` anchors that live links still reach, directly or through
    /// each other, with each one's own last link: unpainted places in the paint walk.
    pub(super) held: BTreeMap<Handle, Option<Handle>>,
    /// Shrink-to-fit scales inside placed transforms: each fitted diagram's own, and the product
    /// a cut composed into an object it re-rooted. Paint work divides them out.
    pub(super) fits: BTreeMap<Handle, f32>,
}
impl fmt::Debug for LayoutState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LayoutState")
            .field("revision", &self.revision)
            .field("objects", &self.objects.len())
            .finish_non_exhaustive()
    }
}
impl LayoutState {
    /// Creates an empty preparation prefix bound to this scene identity.
    pub fn new(scene: &SceneState) -> Self {
        Self {
            owner: scene.preparation_owner(),
            cut_revision: scene.preparation_cut(),
            revision: 0,
            version: Arc::new(()),
            objects: BTreeMap::new(),
            handles: BTreeMap::new(),
            extras: BTreeMap::new(),
            views: BTreeMap::new(),
            held: BTreeMap::new(),
            fits: BTreeMap::new(),
            canvas: Rect {
                origin: Vec2::default(),
                size: scene.header().size.map_or(
                    Vec2::new(super::DEFAULT_CANVAS[0], super::DEFAULT_CANVAS[1]),
                    |size| Vec2::new(size[0].get(), size[1].get()),
                ),
            },
            declared_canvas: scene.header().size.is_some(),
            content: None,
            presentation_scale: crate::limits::presentation_scale(scene.header().surface),
            presentation_limit: crate::limits::presentation_scale(scene.header().surface),
        }
    }
    /// Prepared 3D views by view id.
    pub fn views(&self) -> &BTreeMap<WireId, Arc<ViewGeometry>> {
        &self.views
    }
    /// Whether `id` names a prepared 3D view or solid.
    pub(crate) fn is_three(&self, id: &WireId) -> Option<Handle> {
        self.views.iter().find_map(|(view, geometry)| {
            if view == id {
                return Some(geometry.handle);
            }
            geometry
                .solids
                .iter()
                .find(|solid| solid.id == *id)
                .map(|solid| solid.handle)
        })
    }
    /// The private facts the step that created `handle` compiled for it.
    pub(super) fn extra(&self, handle: Handle) -> Option<&Extra> {
        self.extras.get(&handle.revision())?.get(&handle.slot())
    }
    /// Drops compiled facts once nothing their step created survives.
    pub(super) fn prune_extras(&mut self) {
        if self.extras.is_empty() {
            return;
        }
        let live: BTreeSet<u32> = self
            .handles
            .keys()
            .map(|handle| handle.revision())
            .collect();
        self.extras.retain(|revision, _| live.contains(revision));
        if self.extras.is_empty() {
            self.extras = BTreeMap::new();
        }
    }
    /// Every placed item with its resting values and inherited opacity, for activation.
    /// Each object's item, rest, inherited opacity, label start, paint scale and behind link.
    pub(crate) fn placed(&self) -> impl Iterator<Item = PlacedEntry<'_>> {
        self.objects.values().map(|object| {
            let primitives = &object.item.primitives;
            let label_start = if matches!(&*object.source, Op::Edge(_)) {
                primitives
                    .iter()
                    .position(|primitive| matches!(primitive, crate::display::Prim::Rect { .. }))
                    .unwrap_or(primitives.len())
            } else {
                primitives.len()
            };
            (
                &object.item,
                object.rest,
                object.inherited_opacity,
                label_start,
                object.paint_scale,
                object.behind,
            )
        })
    }
    /// Last prepared or fenced commit revision, including gaps after a cut.
    pub fn revision(&self) -> u32 {
        self.revision
    }
    /// Requested bytes this prefix retains, an allowance pinned to Rust 1.98's layouts rather
    /// than a heap bound: its own index, plus each shared primitive array, vertex array, chart
    /// basis, plot, source copy, compiled facts and motion map once with its `Arc` header. It walks every
    /// object, so compute it once per new rest, not per frame.
    pub fn resident_bytes(&self) -> usize {
        let items = self.objects.values().map(|object| &object.item);
        super::prepare::residency(std::iter::empty(), items)
            + super::chart::retained_bytes(self)
            + heap::shared_bytes(std::iter::empty(), self.objects.values())
            + heap::extras_bytes(std::iter::empty(), self.extras.values())
            + self
                .views
                .values()
                .map(|view| view.full_bytes())
                .sum::<usize>()
            + self.index_bytes()
    }
    /// The index every prepared result holds its own copy of: both B-trees at their node bound,
    /// the two heap copies of each object's id and the version allocation.
    pub(crate) fn index_bytes(&self) -> usize {
        let ids: usize = self.objects.keys().map(|id| id.as_str().len()).sum();
        heap::btree::<WireId, Placed>(self.objects.len())
            + heap::btree::<Handle, WireId>(self.handles.len())
            + heap::btree::<u32, Arc<BTreeMap<usize, Extra>>>(self.extras.len())
            + heap::btree::<WireId, Arc<ViewGeometry>>(self.views.len())
            + heap::btree::<Handle, Option<Handle>>(self.held.len())
            + heap::btree::<Handle, f32>(self.fits.len())
            + self.views.keys().map(|id| id.as_str().len()).sum::<usize>()
            + 2 * ids
            + heap::ARC_HEADER
    }
    /// Paint work of every placed item, summed in O(objects); preparation counted each item.
    pub fn paint_work(&self) -> PaintWork {
        self.objects
            .values()
            .fold(PaintWork::default(), |sum, object| sum + object.work)
    }
    /// Every placed handle in order: what a Stop at rest reveals, without building a view.
    pub fn revealed_at_rest(&self) -> Vec<Handle> {
        let mut handles: Vec<Handle> = self
            .handles
            .keys()
            .copied()
            .chain(self.three_handles())
            .collect();
        handles.sort_unstable();
        handles
    }
    /// Every prepared 3D view and solid; nothing moves them before slice 5's motion sampling.
    pub(crate) fn three_handles(&self) -> impl Iterator<Item = Handle> {
        self.views.values().flat_map(|view| {
            std::iter::once(view.handle).chain(view.solids.iter().map(|solid| solid.handle))
        })
    }
    /// Rebuilds `held` from the live links: every dropped anchor they reach keeps its own last
    /// link from `before`, and one nothing reaches any more is let go.
    pub(super) fn hold_dropped_anchors(&mut self, before: &Self) {
        let mut held = BTreeMap::new();
        for object in self.objects.values() {
            let mut next = object.behind;
            while let Some(at) =
                next.filter(|at| !self.handles.contains_key(at) && !held.contains_key(at))
            {
                next = before
                    .handles
                    .get(&at)
                    .and_then(|id| before.objects.get(id))
                    .map_or_else(
                        || before.held.get(&at).copied().flatten(),
                        |anchor| anchor.behind,
                    );
                held.insert(at, next);
            }
        }
        self.held = held;
    }
    /// Removed or cut anchors still in the paint walk, with their own last links.
    pub(crate) fn held(&self) -> &BTreeMap<Handle, Option<Handle>> {
        &self.held
    }
    /// Resolves a prepared model id without consulting future scene commits.
    pub fn handle(&self, id: &WireId) -> Option<Handle> {
        self.objects.get(id).map(|object| object.item.handle)
    }
    /// Resting bounds of a prepared object, including retained ancestor transforms.
    /// # Errors
    /// Rejects unsafe composed geometry.
    pub fn bounds(&self, id: &WireId) -> Result<Option<crate::geom::Rect>, PrepareError> {
        self.objects
            .get(id)
            .map(|object| self.world_bounds(object))
            .transpose()
    }
    /// Current prepared items in deterministic handle order.
    pub fn items(&self) -> Vec<Item> {
        let mut items: Vec<_> = self
            .objects
            .values()
            .map(|object| object.item.clone())
            .collect();
        items.sort_by_key(|item| item.handle);
        items
    }
    pub(super) fn resting_transform(&self, handle: Handle) -> Result<Affine, PrepareError> {
        let mut chain = Vec::new();
        let mut next = Some(handle);
        while let Some(handle) = next {
            let object = self
                .handles
                .get(&handle)
                .and_then(|id| self.objects.get(id))
                .ok_or_else(|| primitives::layout_error(ErrorCode::UnknownRef))?;
            chain.push(object);
            next = object.item.parent;
            if chain.len() > crate::limits::MAX_OBJECT_DEPTH {
                return Err(primitives::layout_error(ErrorCode::TooDeep));
            }
        }
        Ok(chain
            .into_iter()
            .rev()
            .fold(Affine::default(), |transform, object| {
                transform
                    .compose(object.item.transform)
                    .compose(object.rest.transform())
            }))
    }
    pub(super) fn world_bounds(&self, object: &Placed) -> Result<crate::geom::Rect, PrepareError> {
        let bounds = self
            .resting_transform(object.item.handle)?
            .bounds(object.item.bounds);
        if !bounds.within_canvas() {
            return Err(primitives::layout_error(ErrorCode::OutOfRange));
        }
        Ok(bounds)
    }
    /// Applies semantic Stop's result to the last activated `after` state.
    /// # Errors
    /// Rejects another scene's state, a layout prepared past the kept prefix, a missing
    /// survivor or unsafe frozen geometry.
    pub fn cut(&self, scene: &SceneState) -> Result<Self, PrepareError> {
        if !Arc::ptr_eq(&self.owner, &scene.preparation_owner()) {
            return Err(PrepareError::Stale);
        }
        if self.revision > self.cut_revision.max(scene.activated()) {
            return Err(primitives::layout_error(ErrorCode::OutOfRange));
        }
        // The kept prefix is a subset of this one, so this index bounds the partial map's charge.
        let mut retained = Deferred::new(BTreeMap::new(), self.index_bytes());
        let mut fits = BTreeMap::new();
        for (id, handle, parent) in scene.preparation_objects() {
            let mut object = self
                .objects
                .get(id)
                .filter(|object| object.item.handle == handle)
                .cloned()
                .ok_or(PrepareError::Stale)?;
            let mut fit = self.fits.get(&handle).copied().unwrap_or(1.0);
            if parent.is_none()
                && let Some(old_parent) = object.item.parent
            {
                object.item.transform = self
                    .resting_transform(old_parent)?
                    .compose(object.item.transform);
                let mut next = Some(old_parent);
                while let Some(handle) = next {
                    let ancestor = self
                        .handles
                        .get(&handle)
                        .and_then(|id| self.objects.get(id))
                        .ok_or_else(primitives::invariant)?;
                    object.inherited_opacity *= ancestor.inherited_opacity
                        * ancestor.rest.opacity.unwrap_or(ancestor.item.opacity);
                    fit *= self.fits.get(&handle).copied().unwrap_or(1.0);
                    next = ancestor.item.parent;
                }
            }
            if fit < 1.0 {
                fits.insert(object.item.handle, fit);
            }
            object.item.parent = parent;
            retained.insert(id.clone(), object);
        }
        let mut after = retained.map(|objects| Self {
            owner: Arc::clone(&self.owner),
            cut_revision: scene.preparation_cut(),
            revision: scene.revision(),
            version: Arc::new(()),
            handles: objects
                .iter()
                .map(|(id, object)| (object.item.handle, id.clone()))
                .collect(),
            objects,
            extras: self.extras.clone(),
            views: {
                let three = scene.preparation_three();
                self.views
                    .iter()
                    .filter(|(_, view)| three.contains(&view.handle))
                    .map(|(id, view)| (id.clone(), Arc::new(view.retain(|h| three.contains(&h)))))
                    .collect()
            },
            canvas: self.canvas,
            declared_canvas: self.declared_canvas,
            content: None,
            presentation_scale: self.presentation_scale,
            presentation_limit: self.presentation_limit,
            held: BTreeMap::new(),
            fits,
        });
        after.prune_extras();
        after.hold_dropped_anchors(self);
        for object in after.objects.values() {
            after.world_bounds(object)?;
        }
        after.grow_content()?;
        Ok(after.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::{MeasureError, TextMeasure, TextStyle};
    use crate::layout::{PrepareInput, PreparedStep, SurfaceGeometry, prepare, recover};
    use crate::{
        Admission, Frame, FrameItem, GlobalBudget, Record, SceneView, StopCut, parse_record,
        text::Bounded, theme::Theme,
    };
    use std::sync::atomic::AtomicBool;
    struct Measure;
    impl TextMeasure for Measure {
        fn measure(
            &mut self,
            text: &str,
            _style: TextStyle,
            _width: Option<f32>,
        ) -> Result<[f32; 2], MeasureError> {
            Ok([if text == "long" { 180.0 } else { 8.0 }, 10.0])
        }
    }

    fn sample(view: &SceneView, time: u64) -> Vec<FrameItem> {
        let mut frame = Frame::new();
        view.frame(time, &mut frame).expect("sample");
        frame.items().to_vec()
    }

    fn id(name: &str) -> WireId {
        name.to_owned().try_into().expect("id")
    }
    fn picture(
        scene: &mut SceneState,
        budget: &mut GlobalBudget,
        before: &LayoutState,
        source: &str,
    ) -> PreparedStep {
        let Record::Step(step) = parse_record(source.as_bytes()).expect("step") else {
            panic!("step");
        };
        let Admission::New(candidate) = scene.admit(&step, budget).expect("admit") else {
            panic!("new");
        };
        let ticket = scene.commit(candidate, budget).expect("commit").preparation;
        let input = PrepareInput::new(
            ticket,
            before.clone(),
            SurfaceGeometry::default(),
            Arc::new(Theme::default()),
        )
        .expect("input");
        prepare(input, &mut Measure, &AtomicBool::new(false)).expect("picture")
    }
    #[test]
    fn recovery_preserves_the_hidden_state_used_by_the_next_glide() {
        let Record::Begin(header) =
            parse_record(br#"{"begin":{"v":1,"scene":"hidden","surface":"inline","alt":""}}"#)
                .expect("header")
        else {
            panic!("begin");
        };
        let mut scene = SceneState::begin(header.clone()).expect("scene");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let first = picture(
            &mut scene,
            &mut budget,
            &before,
            r#"{"step":{"id":"first","put":[{"diagram":{"id":"d","box":[0,0,360,300],"dir":"down"}},{"node":{"id":"a","in":"d","label":"A"}},{"node":{"id":"b","in":"d","label":"B"}},{"node":{"id":"c","in":"d","label":"C"}}]}}"#,
        );
        let second = picture(
            &mut scene,
            &mut budget,
            &first.after,
            r#"{"step":{"id":"overflow","put":[{"node":{"id":"overflow","in":"d","label":"D"}}]}}"#,
        );
        assert_eq!(second.glides.len(), 1);
        let kept = Bounded::new(
            ["d", "b", "c", "overflow"]
                .map(|name| second.after.handle(&id(name)).expect("handle"))
                .to_vec(),
        )
        .expect("cut");
        scene
            .stop(
                &StopCut {
                    activated: 2,
                    revealed: kept,
                },
                &mut budget,
            )
            .expect("stop");
        let rest = second.after.cut(&scene).expect("cut");
        let (mut restored, replayed) = recover(
            header,
            scene.recovery_log(),
            SurfaceGeometry::default(),
            &Arc::new(Theme::default()),
            &mut Measure,
            &AtomicBool::new(false),
            None,
        )
        .expect("recover");
        let next = r#"{"step":{"id":"next","put":[{"node":{"id":"next","in":"d","label":"N"}}]}}"#;
        let d = id("d");
        let envelope = |layout: &LayoutState| {
            let motion = layout.objects[&d]
                .motion
                .as_ref()
                .expect("diagram envelope")["pos"];
            (motion.min, motion.max)
        };
        let mut live_view = SceneView::new(&rest, rest.revision());
        let mut replay_view = SceneView::new(&replayed, replayed.revision());
        let kept = envelope(&rest);
        assert_eq!(rest.revealed_at_rest().len(), 4);
        let old_picture = sample(&live_view, 0);
        assert_eq!(old_picture, sample(&replay_view, 0));
        let live = picture(&mut scene, &mut budget, &rest, next);
        let replayed = picture(&mut restored, &mut GlobalBudget::default(), &replayed, next);
        let targets: Vec<_> = live.glides.iter().map(|glide| glide.target).collect();
        assert_eq!(targets, [rest.handle(&d).expect("diagram handle")]);
        assert_eq!(
            envelope(&rest),
            kept,
            "the kept prefix's envelope is unchanged"
        );
        assert_eq!(
            sample(&SceneView::new(&rest, rest.revision()), 0),
            old_picture
        );
        let handle = rest.handle(&id("b")).expect("a retained node");
        let transform = |items: &[FrameItem]| {
            items
                .iter()
                .find(|item| item.handle == handle)
                .expect("retained node paints")
                .transform
        };
        for (view, step) in [(&mut live_view, live), (&mut replay_view, replayed)] {
            view.activate(step, 1000, 0, &mut Frame::new())
                .expect("activate continuation");
        }
        let positions = [1000, 1225, 1450].map(|time| {
            let live = sample(&live_view, time);
            assert_eq!(live, sample(&replay_view, time));
            transform(&live)
        });
        assert_eq!(positions[0], transform(&old_picture));
        assert_ne!(positions[0], positions[1], "the retained node glides");
        assert_ne!(positions[1], positions[2], "the glide keeps progressing");
    }
    #[test]
    fn edges_inside_child_clusters_do_not_count_toward_the_parents_edge_cap() {
        let Record::Begin(header) =
            parse_record(br#"{"begin":{"v":1,"scene":"caps","surface":"inline","alt":""}}"#)
                .expect("header")
        else {
            panic!("begin");
        };
        let mut scene = SceneState::begin(header).expect("scene");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let mut put = vec![r#"{"diagram":{"id":"r"}}"#.to_owned()];
        for group in ["a", "b"] {
            put.push(format!(r#"{{"diagram":{{"id":"{group}","in":"r"}}}}"#));
            for node in ["1", "2"] {
                put.push(format!(
                    r#"{{"node":{{"id":"{group}{node}","in":"{group}","label":"N"}}}}"#
                ));
            }
            for edge in 0..crate::limits::MAX_DIAGRAM_EDGES {
                put.push(format!(
                    r#"{{"edge":{{"id":"{group}e{edge}","in":"{group}","from":"{group}1","to":"{group}2"}}}}"#
                ));
            }
        }
        let source = format!(r#"{{"step":{{"id":"full","put":[{}]}}}}"#, put.join(","));
        let prepared = picture(&mut scene, &mut budget, &before, &source);
        assert_eq!(
            prepared.after.items().len(),
            3 + 4 + 2 * crate::limits::MAX_DIAGRAM_EDGES
        );
    }
    #[test]
    fn initial_sibling_clusters_reserve_their_long_edge_labels() {
        let Record::Begin(header) =
            parse_record(br#"{"begin":{"v":1,"scene":"labels","surface":"inline","alt":""}}"#)
                .expect("header")
        else {
            panic!("begin");
        };
        let mut scene = SceneState::begin(header).expect("scene");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let mut put = vec![r#"{"diagram":{"id":"r","box":[0,0,360,300]}}"#.to_owned()];
        for group in ["a", "b"] {
            put.push(format!(r#"{{"diagram":{{"id":"{group}","in":"r"}}}}"#));
            for node in ["1", "2"] {
                put.push(format!(
                    r#"{{"node":{{"id":"{group}{node}","in":"{group}","label":"N"}}}}"#
                ));
            }
            put.push(format!(
                r#"{{"edge":{{"id":"{group}e","in":"{group}","from":"{group}1","to":"{group}2","label":"long"}}}}"#
            ));
        }
        let source = format!(r#"{{"step":{{"id":"first","put":[{}]}}}}"#, put.join(","));
        let prepared = picture(&mut scene, &mut budget, &before, &source);
        let bounds = ["a", "b"].map(|group| {
            prepared
                .after
                .bounds(&id(group))
                .expect("safe")
                .expect("group")
        });
        assert_eq!(bounds[0].size, Vec2::new(188.0, 152.0));
        assert_eq!(
            bounds[1].origin.x.to_bits(),
            (bounds[0].max().x + 62.0).to_bits()
        );
        let contains = |outer: Rect, inner: Rect| {
            outer.origin.x <= inner.origin.x
                && outer.origin.y <= inner.origin.y
                && outer.max().x >= inner.max().x
                && outer.max().y >= inner.max().y
        };
        for (index, edge) in ["ae", "be"].into_iter().enumerate() {
            let edge = &prepared.after.objects[&id(edge)].item;
            let transform = prepared
                .after
                .resting_transform(edge.handle)
                .expect("safe transform");
            let ink = edge
                .primitives
                .iter()
                .find_map(|p| match p {
                    Prim::Text {
                        origin,
                        text,
                        max_width: Some(width),
                        ..
                    } if text.as_str() == "long" => Some(Rect {
                        origin: *origin + Vec2::new((*width - 180.0) * 0.5, 0.0),
                        size: Vec2::new(180.0, 10.0),
                    }),
                    _ => None,
                })
                .expect("long label");
            let ink = transform.bounds(ink);
            assert!(
                contains(bounds[index], ink),
                "the cluster contains its long edge label"
            );
            assert!(
                !bounds[1 - index].intersects(ink),
                "the label clears the sibling cluster"
            );
        }
    }
    #[test]
    fn cluster_edges_end_on_their_endpoints_resting_bounds() {
        let Record::Begin(header) =
            parse_record(br#"{"begin":{"v":1,"scene":"rests","surface":"inline","alt":""}}"#)
                .expect("header")
        else {
            panic!("begin");
        };
        let mut scene = SceneState::begin(header).expect("scene");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let prepared = picture(
            &mut scene,
            &mut budget,
            &before,
            r#"{"step":{"id":"first","put":[{"diagram":{"id":"r"}},{"diagram":{"id":"g","in":"r"}},{"node":{"id":"a","in":"g","label":"A"}},{"node":{"id":"b","in":"g","label":"B"}},{"edge":{"id":"e","in":"g","from":"a","to":"b"}},{"anim":{"target":"b","track":"y","keys":[[0,100]]}}]}}"#,
        );
        let after = &prepared.after;
        let handle = after.handle(&id("e")).expect("edge handle");
        let frame = sample(&SceneView::new(after, after.revision()), 0);
        let edge = frame
            .iter()
            .find(|item| item.handle == handle)
            .expect("painted edge");
        assert!(edge.opacity > 0.0 && edge.draw > 0.0, "edge is visible");
        let stroke = connected_stroke::connected_stroke(&edge.primitives)
            .expect("connected painted edge stroke");
        let a = after.bounds(&id("a")).expect("safe").expect("a");
        let b = after.bounds(&id("b")).expect("safe").expect("b");
        let ends = [stroke[0], stroke[stroke.len() - 1]].map(|point| edge.transform.point(point));
        let start = Vec2::new(a.center().x, a.max().y);
        let end = Vec2::new(b.center().x, b.origin.y);
        assert!(
            ends == [start, end] || ends == [end, start],
            "edge endpoints {ends:?} connect the resting ports {start:?} and {end:?}"
        );
    }
}
