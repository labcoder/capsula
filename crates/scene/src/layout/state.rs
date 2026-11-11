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
    /// Paint work of `item.primitives`, set once at the step that built them.
    pub work: PaintWork,
}

impl Placed {
    pub(super) fn new(mut item: Item, source: Arc<Op>, cached: Option<&Self>) -> Self {
        if let Some(old) = cached {
            item.transform = old.item.transform;
        }
        Self {
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
    pub(crate) fn placed(&self) -> impl Iterator<Item = (&Item, Rest, f32, usize)> {
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
        for (id, handle, parent) in scene.preparation_objects() {
            let mut object = self
                .objects
                .get(id)
                .filter(|object| object.item.handle == handle)
                .cloned()
                .ok_or(PrepareError::Stale)?;
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
                    next = ancestor.item.parent;
                }
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
        });
        after.prune_extras();
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
        Admission, GlobalBudget, Record, StopCut, parse_record, text::Bounded, theme::Theme,
    };
    use std::sync::atomic::AtomicBool;
    struct Measure;
    impl TextMeasure for Measure {
        fn measure(
            &mut self,
            _text: &str,
            _style: TextStyle,
            _width: Option<f32>,
        ) -> Result<[f32; 2], MeasureError> {
            Ok([8.0, 10.0])
        }
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
    fn a_prepared_step_keeps_no_view_update_buffer() {
        let Record::Begin(header) =
            parse_record(br#"{"begin":{"v":1,"scene":"v","surface":"overlay","alt":""}}"#)
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
            r#"{"step":{"id":"a","put":[{"view3d":{"id":"v"}},{"solid":{"in":"v","id":"s","shape":"box","size":[2,2,2]}}]}}"#,
        );
        assert_eq!(prepared.views.len(), 1);
        assert_eq!(
            prepared.ticket.views.capacity(),
            0,
            "the consumed inputs leave no allocation"
        );
    }
    #[test]
    fn chart_basis_is_replaced_when_attached_and_shared_after_a_cut() {
        let Record::Begin(header) =
            parse_record(br#"{"begin":{"v":1,"scene":"basis","surface":"inline","alt":""}}"#)
                .expect("header")
        else {
            panic!("begin");
        };
        let mut scene = SceneState::begin(header).expect("scene");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let first = picture(
            &mut scene,
            &mut budget,
            &before,
            r#"{"step":{"id":"first","put":[{"chart":{"id":"c","kind":"bar","stacked":true,"box":[0,0,400,300]}},{"series":{"id":"s","in":"c","data":[1,2]}}]}}"#,
        );
        let key: WireId = "s".to_owned().try_into().expect("id");
        let first_basis = first.after.objects[&key]
            .chart_basis
            .as_ref()
            .expect("basis");
        let second = picture(
            &mut scene,
            &mut budget,
            &first.after,
            r#"{"step":{"id":"color","set":[{"target":"s","color":"red"}]}}"#,
        );
        let second_basis = second.after.objects[&key]
            .chart_basis
            .as_ref()
            .expect("refitted basis");
        assert!(
            !Arc::ptr_eq(first_basis, second_basis),
            "attached preparation installs a new immutable basis"
        );
        let handle = second.after.handle(&key).expect("series handle");
        scene
            .stop(
                &StopCut {
                    activated: 2,
                    revealed: Bounded::new(vec![handle]).expect("cut"),
                },
                &mut budget,
            )
            .expect("stop");
        let detached = second.after.cut(&scene).expect("detached layout");
        let detached_basis = detached.objects[&key]
            .chart_basis
            .as_ref()
            .expect("frozen basis");
        assert!(
            Arc::ptr_eq(second_basis, detached_basis),
            "the cut carries the existing allocation"
        );
        let next = picture(
            &mut scene,
            &mut budget,
            &detached,
            r#"{"step":{"id":"data","set":[{"target":"s","data":[2,1]}]}}"#,
        );
        assert!(
            Arc::ptr_eq(
                detached_basis,
                next.after.objects[&key]
                    .chart_basis
                    .as_ref()
                    .expect("updated basis")
            ),
            "detached updates reuse the allocation rather than refitting"
        );
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
                .map(|id| {
                    second
                        .after
                        .handle(&id.to_owned().try_into().expect("id"))
                        .expect("handle")
                })
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
        assert_eq!(rest.objects.len(), 4);
        for (id, object) in &rest.objects {
            let replayed = &replayed.objects[id];
            assert_eq!(object.rest, replayed.rest);
            assert_eq!(object.rank, replayed.rank);
            assert_eq!(object.motion, replayed.motion);
        }
        let next = r#"{"step":{"id":"next","put":[{"node":{"id":"next","in":"d","label":"N"}}]}}"#;
        let d: WireId = "d".to_owned().try_into().expect("id");
        let kept = rest.objects[&d].motion.as_deref().cloned();
        let live = picture(&mut scene, &mut budget, &rest, next);
        let replayed = picture(&mut restored, &mut GlobalBudget::default(), &replayed, next);
        assert_eq!(live.after.items(), replayed.after.items());
        assert_eq!(live.glides, replayed.glides);
        assert_eq!(live.glides.len(), 1);
        let motion = |state: &LayoutState| {
            Arc::clone(
                state.objects[&d]
                    .motion
                    .as_ref()
                    .expect("the diagram glided"),
            )
        };
        assert!(
            !Arc::ptr_eq(&motion(&rest), &motion(&live.after)),
            "the next glide writes a copy of the shared map"
        );
        assert_eq!(
            rest.objects[&d].motion.as_deref(),
            kept.as_ref(),
            "the kept prefix's envelope is unchanged"
        );
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
        struct Wide;
        impl TextMeasure for Wide {
            fn measure(
                &mut self,
                text: &str,
                _style: TextStyle,
                _width: Option<f32>,
            ) -> Result<[f32; 2], MeasureError> {
                Ok([if text == "long" { 180.0 } else { 8.0 }, 10.0])
            }
        }
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
        let Record::Step(step) = parse_record(source.as_bytes()).expect("step") else {
            panic!("step");
        };
        let Admission::New(candidate) = scene.admit(&step, &budget).expect("admit") else {
            panic!("new");
        };
        let ticket = scene
            .commit(candidate, &mut budget)
            .expect("commit")
            .preparation;
        let input = PrepareInput::new(
            ticket,
            before,
            SurfaceGeometry::default(),
            Arc::new(Theme::default()),
        )
        .expect("input");
        let prepared = prepare(input, &mut Wide, &AtomicBool::new(false)).expect("prepared");
        let bounds = ["a", "b"].map(|group| {
            prepared
                .after
                .bounds(&group.to_owned().try_into().expect("id"))
                .expect("safe")
                .expect("group")
        });
        assert_eq!(bounds[0].size.x.to_bits(), 188.0_f32.to_bits());
        assert_eq!(bounds[0].size.y.to_bits(), 152.0_f32.to_bits());
        assert_eq!(
            bounds[1].origin.x.to_bits(),
            (bounds[0].max().x + 62.0).to_bits()
        );
        assert!(
            !bounds[0].intersects(bounds[1]),
            "sibling groups stay apart"
        );
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
        let id = |name: &str| -> WireId { name.to_owned().try_into().expect("id") };
        let after = &prepared.after;
        let edge = &after.objects[&id("e")];
        let end = after
            .resting_transform(edge.item.parent.expect("the edge sits in g"))
            .expect("safe")
            .point(edge.anchors[1]);
        let b = after.bounds(&id("b")).expect("safe").expect("b");
        assert_eq!(end.y.to_bits(), b.origin.y.to_bits());
        assert_eq!(end.x.to_bits(), (b.origin.x + 40.0).to_bits());
    }
}
