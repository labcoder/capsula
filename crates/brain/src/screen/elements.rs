//! Bounded, capture-scoped screen text; the platform only supplies cached nodes.

use std::time::Instant;

use kumochi_protocol::geometry::{PhysicalRect, WindowRef};
use serde::{Deserialize, Serialize};

use super::capture::Failure;
use super::mask::Completeness;
#[cfg(test)]
use super::mask::MaskSet;
pub(crate) use super::mask::{MaskSink, Unavailable};
use super::wire::CaptureId;

/// Which top-level window supplies the control view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ElementTarget {
    /// Use the foreground window pinned by the capture.
    Foreground,
    /// A visible top-level window, validated at walk time.
    Window {
        /// Opaque native reference.
        window: WindowRef,
        /// Process that owned it when selected.
        pid: u32,
    },
}

/// One capture's bounded control view. Elevated, Busy, Excluded and `NoWindow` mean masks are unknown;
/// consumers must withhold or flag the image, never treat it as fully masked.
#[derive(PartialEq, Eq, Serialize, Deserialize)]
pub struct ElementTree {
    /// The authority that was spent for this walk.
    pub capture: CaptureId,
    /// Physical top-level window bounds.
    pub window: PhysicalRect,
    /// Pre-order nodes, with capture-scoped ids.
    pub elements: Vec<Element>,
    /// Why the walk stopped early.
    pub cut: Option<Cut>,
    /// Whether the root could be read.
    pub availability: Availability,
}

/// A cached node, with no password name.
#[derive(PartialEq, Eq, Serialize, Deserialize)]
pub struct Element {
    /// Capture-scoped identifier.
    pub id: ElementId,
    /// Parent's index, or none for the root.
    pub parent: Option<u16>,
    /// At most 128 characters; empty for passwords.
    pub name: String,
    /// Whether the name exceeded the character cap.
    pub name_cut: bool,
    /// Whether the node must be masked.
    pub password: bool,
    /// The platform's numeric control type.
    pub control_type: i32,
    /// Physical desktop bounds.
    pub screen: PhysicalRect,
    /// The same bounds relative to the window's origin.
    pub in_window: PhysicalRect,
}

/// Serialized as `c12.e7`, meaningful only within its capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElementId {
    /// Owning capture.
    pub capture: CaptureId,
    /// Pre-order position.
    pub index: u16,
}

impl Serialize for ElementId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("c{}.e{}", self.capture.0, self.index))
    }
}

impl<'de> Deserialize<'de> for ElementId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        let parsed = value
            .strip_prefix('c')
            .and_then(|v| v.split_once(".e"))
            .and_then(|(c, e)| {
                Some(Self {
                    capture: CaptureId(c.parse().ok()?),
                    index: e.parse().ok()?,
                })
            });
        parsed
            .filter(|id| value == format!("c{}.e{}", id.capture.0, id.index))
            .ok_or_else(|| serde::de::Error::custom("invalid element id"))
    }
}

/// A partial tree's stopping condition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cut {
    /// The next element would exceed 512.
    Elements,
    /// Children below depth 16 were skipped.
    Depth,
    /// Element JSON would exceed 60,000 bytes.
    Bytes,
    /// The cooperative deadline fired.
    Budget,
    /// A provider failed or stopped answering.
    Provider,
}

/// Whether the window's root was readable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Availability {
    /// The window resolved; an empty or partial tree is explained by `cut`.
    Read,
    /// No content was available.
    Unavailable(Unavailable),
}

impl std::fmt::Debug for ElementTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ElementTree")
            .field("elements", &self.elements.len())
            .field("cut", &self.cut)
            .field("availability", &self.availability)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Element {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Element")
            .field("name_chars", &self.name.chars().count())
            .field("password", &self.password)
            .finish_non_exhaustive()
    }
}

pub(crate) struct NodeProps {
    pub(crate) name: String,
    pub(crate) password: bool,
    pub(crate) control_type: i32,
    pub(crate) rect: PhysicalRect,
}

impl std::fmt::Debug for NodeProps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeProps")
            .field(
                "name_chars",
                &if self.password {
                    0
                } else {
                    self.name.chars().count()
                },
            )
            .field("password", &self.password)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SourceError {
    #[error("elevated window")]
    Elevated,
    #[error("no window")]
    NoWindow,
    #[error("excluded window")]
    Excluded,
    #[error("element source failed ({0})")]
    Fault(i32),
}

impl std::fmt::Debug for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

pub(crate) trait ElementSource {
    type Node;
    /// Returns the native window owner's pid, as foreground does, not a hosted UIA provider's pid.
    fn root(
        &mut self,
        target: ElementTarget,
    ) -> Result<(Self::Node, PhysicalRect, u32), SourceError>;
    fn read(&mut self, node: &Self::Node) -> Result<NodeProps, SourceError>;
    fn mask(&mut self, node: &Self::Node) -> Result<(bool, PhysicalRect), SourceError> {
        self.read(node).map(|props| (props.password, props.rect))
    }
    fn first_child(&mut self, node: &Self::Node) -> Result<Option<Self::Node>, SourceError>;
    fn next_sibling(&mut self, node: &Self::Node) -> Result<Option<Self::Node>, SourceError>;
}

pub(crate) trait ElementReader {
    fn set_shell_pid(&mut self, _pid: u32) {}
    fn foreground(&mut self) -> Option<TopWindow>;
    /// Hands `job` to the walk without waiting. `Err` is the walk's answer when it can't start;
    /// a job still pending, even one whose completion landed untaken, retires the reader.
    fn begin(&mut self, job: WalkJob) -> Result<(), Walked>;
    /// The pending walk's answer once its completion landed, which the opener's [`Wake`]
    /// announces; `None` while it runs or when nothing is pending.
    #[cfg_attr(not(test), expect(dead_code, reason = "A6b2's actor polls"))]
    fn poll(&mut self) -> Option<Result<Walked, Failure>>;
    fn walk(&mut self, job: WalkJob) -> Result<Walked, Failure>;
}

/// Called after each walk completion lands; it never blocks, and `false` means nobody listens.
pub(crate) type Wake = Box<dyn Fn() -> bool + Send>;

pub(crate) type OpenElements = fn(Wake) -> Result<Box<dyn ElementReader>, i32>;

pub(crate) struct ElementLimits {
    pub(crate) elements: usize,
    pub(crate) depth: usize,
    pub(crate) name_chars: usize,
    pub(crate) bytes: usize,
}

// The byte cap usually binds before the element cap.
pub(crate) const LIMITS: ElementLimits = ElementLimits {
    elements: 512,
    depth: 16,
    name_chars: 128,
    bytes: 60_000,
};

const MASK_READ_LIMIT: usize = 2_048;

pub(crate) struct TopWindow {
    pub(crate) window: WindowRef,
    pub(crate) pid: u32,
    pub(crate) rect: PhysicalRect,
}

impl std::fmt::Debug for TopWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TopWindow(..)")
    }
}

pub(crate) trait ElementPlatform: Clone + Send + 'static {
    fn set_shell_pid(&mut self, _pid: u32) {}
    fn foreground(&mut self) -> Option<TopWindow>;
    fn excluded(&mut self, pid: u32) -> Result<bool, SourceError>;
}

pub(crate) struct WalkJob {
    pub(crate) target: ElementTarget,
    pub(crate) capture: CaptureId,
    pub(crate) display: PhysicalRect,
    pub(crate) until: Instant,
    pub(crate) masks: MaskSink,
}

pub(crate) struct Walked {
    pub(crate) tree: ElementTree,
    pub(crate) retire: bool,
}

impl ElementTree {
    pub(crate) fn unavailable(capture: CaptureId, why: Unavailable) -> Self {
        Self {
            capture,
            window: PhysicalRect {
                origin: [0; 2],
                size: [0; 2],
            },
            elements: Vec::new(),
            cut: None,
            availability: Availability::Unavailable(why),
        }
    }
}

pub(super) fn finish_unavailable(masks: &MaskSink, why: Unavailable) -> bool {
    finish_masks(masks, Completeness::Unknown(why))
}

impl std::fmt::Debug for WalkJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WalkJob(..)")
    }
}

impl std::fmt::Debug for Walked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Walked")
            .field("tree", &self.tree)
            .field("retire", &self.retire)
            .finish()
    }
}

fn finish_masks(masks: &MaskSink, completeness: Completeness) -> bool {
    match masks.lock() {
        Ok(mut masks) => {
            // MaskSet keeps the first finish; this also drops a late walk's text reply.
            if masks.completeness() == Completeness::Unknown(Unavailable::Busy) {
                return false;
            }
            masks.finish(completeness);
            true
        }
        Err(poisoned) => {
            poisoned
                .into_inner()
                .finish(Completeness::Unknown(Unavailable::Busy));
            false
        }
    }
}

pub(crate) fn excluded_image(image: &str) -> bool {
    let name = image.rsplit(['/', '\\']).next().unwrap_or(image);
    [
        kumochi_protocol::constants::SHELL_EXE,
        kumochi_protocol::constants::BRAIN_EXE,
    ]
    .iter()
    .any(|binary| name.eq_ignore_ascii_case(binary))
}

pub(crate) fn intersects(a: PhysicalRect, b: PhysicalRect) -> bool {
    (0..2).all(|axis| {
        a.size[axis] > 0
            && b.size[axis] > 0
            && i64::from(a.origin[axis]) < i64::from(b.origin[axis]) + i64::from(b.size[axis])
            && i64::from(b.origin[axis]) < i64::from(a.origin[axis]) + i64::from(a.size[axis])
    })
}

pub(crate) fn walk<S: ElementSource>(
    source: &mut S,
    job: &WalkJob,
    limits: &ElementLimits,
    now: &dyn Fn() -> Instant,
    platform: &mut impl ElementPlatform,
) -> Result<ElementTree, Failure> {
    if !job
        .masks
        .lock()
        .is_ok_and(|masks| masks.capture() == job.capture)
    {
        finish_unavailable(&job.masks, Unavailable::Busy);
        return Ok(ElementTree::unavailable(job.capture, Unavailable::Busy));
    }
    let result = walk_tree(source, job, limits, now, platform);
    match result {
        Ok((tree, completeness)) if finish_masks(&job.masks, completeness) => Ok(tree),
        Ok(_) => Ok(ElementTree::unavailable(job.capture, Unavailable::Busy)),
        Err(error) => {
            finish_unavailable(&job.masks, Unavailable::Busy);
            Err(error)
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one bounded pre-order walk keeps its limits and provider failure handling together"
)]
fn walk_tree<S: ElementSource>(
    source: &mut S,
    job: &WalkJob,
    limits: &ElementLimits,
    now: &dyn Fn() -> Instant,
    platform: &mut impl ElementPlatform,
) -> Result<(ElementTree, Completeness), Failure> {
    let WalkJob {
        target,
        capture,
        display,
        until,
        masks,
    } = job;
    let capture = *capture;
    let mut tree = ElementTree {
        capture,
        window: PhysicalRect {
            origin: [0; 2],
            size: [0; 2],
        },
        elements: Vec::new(),
        cut: None,
        availability: Availability::Read,
    };
    let rooted = source.root(*target).and_then(|(root, window, pid)| {
        if let ElementTarget::Window { pid: hint, .. } = target
            && pid != *hint
        {
            return Err(SourceError::NoWindow);
        }
        if !intersects(window, *display) {
            return Err(SourceError::NoWindow);
        }
        if platform.excluded(pid)? {
            return Err(SourceError::Excluded);
        }
        Ok((root, window))
    });
    let root = match rooted {
        Ok((root, window)) => {
            tree.window = window;
            root
        }
        Err(SourceError::Fault(code)) => return Err(Failure::Fault(code)),
        Err(reason) => {
            let why = match reason {
                SourceError::Elevated => Unavailable::Elevated,
                SourceError::Excluded => Unavailable::Excluded,
                _ => Unavailable::NoWindow,
            };
            return Ok((
                ElementTree::unavailable(capture, why),
                Completeness::Unknown(why),
            ));
        }
    };
    let mut stack = vec![(root, None, 0)];
    let mut bytes = 0;
    let mut mask_only = false;
    let mut mask_reads = 0;
    let mut complete = true;
    while let Some((node, parent, depth)) = stack.pop() {
        let stop = if !mask_only && tree.elements.len() >= limits.elements {
            Some(Cut::Elements)
        } else if !mask_only && bytes >= limits.bytes {
            Some(Cut::Bytes)
        } else {
            None
        };
        if stop.is_some() {
            tree.cut = stop;
            mask_only = true;
        }
        if now() >= *until || (mask_only && mask_reads >= MASK_READ_LIMIT) {
            if !mask_only {
                tree.cut = Some(Cut::Budget);
            }
            complete = false;
            break;
        }
        mask_reads += usize::from(mask_only);
        let read = if mask_only {
            source.mask(&node).map(|(password, rect)| NodeProps {
                name: String::new(),
                password,
                rect,
                control_type: 0,
            })
        } else {
            source.read(&node)
        };
        let Ok(props) = read else {
            if !mask_only {
                tree.cut = Some(Cut::Provider);
            }
            complete = false;
            break;
        };
        if props.password {
            match masks.lock() {
                Ok(mut masks) => masks.push(props.rect),
                Err(_) => {
                    return Ok((
                        ElementTree::unavailable(capture, Unavailable::Busy),
                        Completeness::Unknown(Unavailable::Busy),
                    ));
                }
            }
            complete &= !props.rect.size.contains(&0);
        }
        let index = u16::try_from(tree.elements.len()).map_err(|_| Failure::Fault(0))?;
        if !mask_only {
            let visible = !props.password && intersects(props.rect, *display);
            let name_cut = visible && props.name.chars().count() > limits.name_chars;
            let element = Element {
                id: ElementId { capture, index },
                parent,
                name: if visible {
                    props.name.chars().take(limits.name_chars).collect()
                } else {
                    String::new()
                },
                name_cut,
                password: props.password,
                control_type: props.control_type,
                screen: props.rect,
                in_window: PhysicalRect {
                    origin: [
                        props.rect.origin[0].saturating_sub(tree.window.origin[0]),
                        props.rect.origin[1].saturating_sub(tree.window.origin[1]),
                    ],
                    size: props.rect.size,
                },
            };
            let len = serde_json::to_vec(&element)
                .map_err(|_| Failure::Fault(0))?
                .len();
            if bytes + len > limits.bytes {
                tree.cut = Some(Cut::Bytes);
                mask_only = true;
            } else {
                bytes += len;
                tree.elements.push(element);
            }
        }
        let navigation = (|| {
            if parent.is_some()
                && let Some(sibling) = source.next_sibling(&node)?
            {
                stack.push((sibling, parent, depth));
            }
            if !props.password
                && let Some(child) = source.first_child(&node)?
            {
                if depth < limits.depth {
                    stack.push((child, Some(index), depth + 1));
                } else {
                    if !mask_only {
                        tree.cut = Some(Cut::Depth);
                    }
                    complete = false;
                }
            }
            Ok::<_, SourceError>(())
        })();
        if navigation.is_err() {
            if !mask_only {
                tree.cut = Some(Cut::Provider);
            }
            complete = false;
            break;
        }
    }
    Ok((
        tree,
        if complete {
            Completeness::Complete
        } else {
            Completeness::Partial
        },
    ))
}

#[cfg(test)]
pub(crate) mod tests;
