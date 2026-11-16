//! The screen surface: what it admits, the brain's stamp, its stored form and its placement.
use crate::{
    Begin, ErrorCode, FieldPath, Op, Step, StepError,
    geom::Affine,
    limits::{MAX_SCREEN_LABEL_CHARS, MAX_SCREEN_SCALE},
    record::{Anchor, ScreenDisplay, Surface, Target},
    text::{Bounded, Finite},
};

/// The box every stored screen mark carries in place of the model's: one the format accepts.
pub const PLACEHOLDER_BOX: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

/// A mark's stroke width, in logical pixels [designer's number].
pub const MARK_STROKE: f32 = 2.0;
/// The contrasting edge on each side of a mark's stroke, in logical pixels [designer's number].
pub const MARK_EDGE: f32 = 1.0;
/// A mark's default colour [designer's number].
pub const MARK_RGB: u32 = 0x006B_5CF0;
/// The colour of a mark's edge [designer's number].
pub const MARK_EDGE_RGB: u32 = 0x00FF_FFFF;
/// Default space between the thing and its mark, in logical pixels [placeholder].
pub const MARK_PAD: f32 = 4.0;
/// Corner radius inside a box mark, in logical pixels [placeholder].
pub const MARK_RADIUS: f32 = 6.0;
/// Corner radius inside a ring around a large thing, in logical pixels [placeholder].
pub const RING_RADIUS: f32 = 12.0;
/// Longest padded side, in logical pixels, a ring still loops round [placeholder].
pub const RING_LOOP_MAX: f32 = 96.0;
/// Space between a mark's outside and its label, in logical pixels [placeholder].
pub const LABEL_GAP: f32 = 6.0;
/// A label's inner padding, in logical pixels [placeholder].
pub const LABEL_PAD: [f32; 2] = [8.0, 5.0];

/// Checks a step of a screen scene: only marks that never cover their target are admitted. A
/// highlight or callout targets a box or a mark, and a label is one short line. Nothing else
/// holds a position, so a box target is the only one.
/// # Errors
/// Returns `Unsupported` at the first other kind or field, `OutOfRange` for a box outside the
/// canvas of `size`, and `LabelTooLong` for a long label.
pub fn admit(step: &Step, size: [f32; 2]) -> Result<(), StepError> {
    let fail = |path, code| Err(StepError::in_step(&step.id, path, code));
    if !step.set.is_empty() {
        return fail(FieldPath::default().field("set"), ErrorCode::Unsupported);
    }
    for (index, op) in step.put.iter().enumerate() {
        let at = |field| FieldPath::default().field("put").index(index).field(field);
        let marked = |target: &Target| !matches!(target, Target::Anchor(Anchor::At(_)));
        let boxed = match op {
            Op::Highlight(mark) => Some(&mark.target),
            Op::Callout(label) => Some(&label.target),
            _ => None,
        };
        if let Some(Target::Box([x, y, width, height])) = boxed
            && (x.get() < 0.0
                || y.get() < 0.0
                || x.get() + width.get() > size[0]
                || y.get() + height.get() > size[1])
        {
            return fail(at("target"), ErrorCode::OutOfRange);
        }
        let refused = match op {
            Op::Highlight(mark) if mark.parent.is_some() => Some(at("in")),
            Op::Highlight(mark) if !marked(&mark.target) => Some(at("target")),
            Op::Callout(label) if label.parent.is_some() => Some(at("in")),
            Op::Callout(label) if !marked(&label.target) => Some(at("target")),
            Op::Callout(label) if label.at.is_some() => Some(at("at")),
            Op::Callout(label) => {
                let text = label.text.as_str();
                if text.chars().count() > MAX_SCREEN_LABEL_CHARS
                    || text.chars().any(char::is_control)
                {
                    return fail(at("text"), ErrorCode::LabelTooLong);
                }
                None
            }
            Op::Highlight(_) => None,
            _ => Some(FieldPath::default().field("put").index(index)),
        };
        if let Some(path) = refused {
            return fail(path, ErrorCode::Unsupported);
        }
    }
    Ok(())
}

/// Stamps a model's screen header with the picture's pixel size and the display it shows.
/// # Errors
/// Refuses another surface or a model-written display as `Unsupported`, and a written size other
/// than the picture's as `SizeNotPicture`.
pub fn stamp(
    begin: &mut Begin,
    picture: [u32; 2],
    display: ScreenDisplay,
) -> Result<(), StepError> {
    let fail = |field, code| {
        Err(StepError {
            step: None,
            path: FieldPath::default().field(field),
            code,
        })
    };
    if begin.surface != Surface::Screen {
        return fail("surface", ErrorCode::Unsupported);
    }
    if begin.display.is_some() {
        return fail("display", ErrorCode::Unsupported);
    }
    let size = picture.map(|side| u16::try_from(side).map_or(f32::INFINITY, f32::from));
    let size = size.map(Finite::try_from);
    let (Ok(width), Ok(height)) = (size[0], size[1]) else {
        return fail("size", ErrorCode::OutOfRange);
    };
    if begin.size.is_some_and(|written| written != [width, height]) {
        return fail("size", ErrorCode::SizeNotPicture);
    }
    begin.size = Some([width, height]);
    begin.display = Some(display);
    Ok(())
}

/// The step as it may be stored: every box target replaced by [`PLACEHOLDER_BOX`].
/// # Panics
/// Never: the placeholder is finite and the step keeps its own number of operations.
#[must_use]
pub fn without_boxes(step: &Step) -> Step {
    let placeholder = || {
        Target::Box(
            PLACEHOLDER_BOX.map(|value| Finite::try_from(value).expect("placeholder is finite")),
        )
    };
    let put = step
        .put
        .iter()
        .map(|op| {
            let mut op = op.clone();
            match &mut op {
                Op::Highlight(mark) if matches!(mark.target, Target::Box(_)) => {
                    mark.target = placeholder();
                }
                Op::Callout(label) if matches!(label.target, Target::Box(_)) => {
                    label.target = placeholder();
                }
                _ => {}
            }
            op
        })
        .collect();
    Step {
        id: step.id.clone(),
        say: step.say.clone(),
        put: Bounded::new(put).expect("as many operations as the step's own"),
        set: step.set.clone(),
        remove: step.remove.clone(),
        start: step.start.clone(),
    }
}

/// Puts a screen canvas of `size` picture pixels on an overlay covering `overlay`, in the
/// overlay's logical pixels: `None` unless it is the display the picture was taken of.
#[must_use]
pub fn placement(display: ScreenDisplay, size: [f32; 2], overlay: ScreenDisplay) -> Option<Affine> {
    if display != overlay {
        return None;
    }
    let [x, y] = logical_per_unit(display, size)?;
    Some(Affine([x, 0.0, 0.0, y, 0.0, 0.0]))
}

/// Logical pixels of `display` per canvas unit on each axis, when that is positive and no larger
/// than a screen scene is prepared for.
pub(crate) fn logical_per_unit(display: ScreenDisplay, size: [f32; 2]) -> Option<[f32; 2]> {
    let dpi = f32::from(u16::try_from(display.dpi).ok().filter(|dpi| *dpi > 0)?);
    let axis = |physical: u32, units: f32| {
        let physical = f32::from(u16::try_from(physical).ok()?);
        let scale = physical / units * 96.0 / dpi;
        (units > 0.0 && scale > 0.0 && scale <= MAX_SCREEN_SCALE).then_some(scale)
    };
    Some([
        axis(display.size[0], size[0])?,
        axis(display.size[1], size[1])?,
    ])
}
