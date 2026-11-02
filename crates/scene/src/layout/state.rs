use super::{PrepareError, primitives};
use crate::{
    ErrorCode, Handle, Op, SceneState, WireId,
    display::Item,
    geom::{Affine, Vec2},
    record::{KeyValue, Track},
};
use std::{collections::BTreeMap, fmt, sync::Arc};

#[derive(Debug, Clone, Copy)]
pub(super) struct Rest {
    pub pos: Vec2,
    pub x: f32,
    pub y: f32,
    pub scale: f32,
    pub rotate: f32,
    pub opacity: f32,
}
impl Default for Rest {
    fn default() -> Self {
        Self {
            pos: Vec2::default(),
            x: 0.0,
            y: 0.0,
            scale: 1.0,
            rotate: 0.0,
            opacity: 1.0,
        }
    }
}
impl Rest {
    pub(super) fn transform(self) -> Affine {
        Affine::pose(
            self.scale,
            self.rotate.to_radians(),
            self.pos + Vec2::new(self.x, self.y),
        )
    }
    pub(super) fn set(&mut self, track: Track, value: &KeyValue) {
        match (track, value) {
            (Track::Pos, KeyValue::V2(v)) => self.pos = Vec2::new(v[0].get(), v[1].get()),
            (Track::X, KeyValue::Num(v)) => self.x = v.get(),
            (Track::Y, KeyValue::Num(v)) => self.y = v.get(),
            (Track::Scale, KeyValue::Num(v)) => self.scale = v.get(),
            (Track::Rotate, KeyValue::Num(v)) => self.rotate = v.get(),
            (Track::Opacity, KeyValue::Num(v)) => self.opacity = v.get(),
            _ => {}
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct Placed {
    pub item: Item,
    pub source: Arc<Op>,
    pub anchors: [Vec2; 2],
    pub rest: Rest,
    pub rank: Option<usize>,
    pub initialized: bool,
    pub motion: BTreeMap<&'static str, crate::state::work::Motion>,
    pub references: Vec<WireId>,
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
        }
    }
    /// Last prepared or fenced commit revision, including gaps after a cut.
    pub fn revision(&self) -> u32 {
        self.revision
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
    /// Rejects another scene's state, a missing survivor or unsafe frozen geometry.
    pub fn cut(&self, scene: &SceneState) -> Result<Self, PrepareError> {
        if !Arc::ptr_eq(&self.owner, &scene.preparation_owner()) {
            return Err(PrepareError::Stale);
        }
        let mut after = self.clone();
        let mut retained = BTreeMap::new();
        for (id, handle, parent, references) in scene.preparation_objects() {
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
                        .ok_or(PrepareError::Stale)?;
                    object.item.opacity *= ancestor.item.opacity * ancestor.rest.opacity;
                    next = ancestor.item.parent;
                }
            }
            object.item.parent = parent;
            object.references = references.to_vec();
            retained.insert(id.clone(), object);
        }
        after.objects = retained;
        after.handles = after
            .objects
            .iter()
            .map(|(id, object)| (object.item.handle, id.clone()))
            .collect();
        after.revision = scene.revision();
        after.cut_revision = scene.preparation_cut();
        after.version = Arc::new(());
        for object in after.objects.values() {
            after.world_bounds(object)?;
        }
        Ok(after)
    }
}
