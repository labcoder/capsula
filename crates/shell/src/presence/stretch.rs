//! The stretch: the bulb follows the pointer while the pointer is over the presence, or a clock
//! script under `--stretch-fixture`, through one critically damped spring. Each frame computes
//! the outline, hands the hit mask that outline plus the previous one, then repaints the dock
//! with the new outline only if a pixel changed. Frames run only while the bulb moves, plus one
//! to trim the mask, so a settled presence asks GPUI for nothing.

use std::f64::consts::TAU;
use std::fmt;
use std::time::Instant;

use gpui_kit::{Context, WeakEntity, Window};

use super::outline::{self, Outline, Ownership};
use super::{PanelVisibility, Presence, diagnostics, frame_rate, observer, scaled};
use crate::platform::{self, MaskUpdate, PresenceError};

/// The bulb's radius, in logical pixels.
const BULB_RADIUS: f32 = 14.0;
/// How far the bulb's center goes from the pill's spine, in logical pixels.
const REACH: f32 = 96.0;
/// How far the outline may grow past the hit proxy before the proxy moves, in logical pixels.
const MARGIN: f32 = 24.0;
/// The pill's inset in its 112 x 48 dock slot.
const PILL_INSET: f32 = 4.0;
/// The spring's natural frequency, per second: a 90 px pull settles in about a third of a
/// second, with no overshoot.
const SPRING: f64 = 28.0;
/// The spring integrates in steps no longer than this, so it moves alike at every frame rate.
const MAX_SUBSTEP: f64 = 1.0 / 144.0;
/// The fixture's out-and-back period, in seconds.
const FIXTURE_PERIOD: f64 = 2.0;
/// Failed uploads in a row before frames stop; a pointer event or position choice rearms them.
const MAX_FAILED_UPLOADS: u8 = 3;

/// What moves the bulb.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Drive {
    /// Back to the pill's center.
    Rest,
    /// Toward the pointer, until it leaves the bulb's reach.
    Cursor,
    /// Out and back on a clock that started then, ignoring the pointer.
    Fixture(Instant),
}

/// Mask updates, with no content: the bench's evidence of what each frame cost Windows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Counts {
    ticks: u64,
    shape: u64,
    moves: u64,
    unchanged: u64,
    failed: u64,
}

impl Counts {
    fn record(&mut self, update: &Result<MaskUpdate, PresenceError>) {
        match update {
            Ok(MaskUpdate::Unchanged) => self.unchanged += 1,
            Ok(MaskUpdate::Shape) => self.shape += 1,
            Ok(MaskUpdate::Moved) => self.moves += 1,
            Err(_) => self.failed += 1,
        }
    }
}

impl fmt::Display for Counts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ticks={} shape={} moves={} unchanged={} failed={}",
            self.ticks, self.shape, self.moves, self.unchanged, self.failed
        )
    }
}

/// The bulb's offset from the pill's center, in physical pixels, and its velocity.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Spring {
    offset: [f64; 2],
    velocity: [f64; 2],
}

impl Spring {
    /// Steps `dt` seconds toward `goal`. Once it's there and still, it snaps exactly onto the
    /// goal and returns true.
    fn step(&mut self, goal: [f64; 2], dt: f64) -> bool {
        let steps = (dt / MAX_SUBSTEP).ceil();
        let dt = dt / steps;
        let mut step = 0.0;
        while step < steps {
            step += 1.0;
            for ((offset, velocity), goal) in
                self.offset.iter_mut().zip(&mut self.velocity).zip(goal)
            {
                let pull = SPRING * SPRING * (goal - *offset) - 2.0 * SPRING * *velocity;
                *velocity += pull * dt;
                *offset += *velocity * dt;
            }
        }
        let near = (goal[0] - self.offset[0]).hypot(goal[1] - self.offset[1]) < 0.25;
        let still = self.velocity[0].hypot(self.velocity[1]) < 4.0;
        if near && still {
            self.offset = goal;
            self.velocity = [0.0; 2];
        }
        near && still
    }
}

/// What the hit mask holds, as far as Windows accepted it, and the failed uploads since.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct MaskState {
    owned: Ownership,
    failed: u8,
}

impl MaskState {
    /// Records one upload of `proposal`. Only an accepted one advances what's owned and drawn.
    fn record(&mut self, proposal: Ownership, upload: &Result<MaskUpdate, PresenceError>) {
        if upload.is_ok() {
            self.owned = proposal;
            self.failed = 0;
        } else {
            self.failed = self.failed.saturating_add(1);
        }
    }

    /// Whether failures in a row have used up the retry frames.
    fn exhausted(&self) -> bool {
        self.failed >= MAX_FAILED_UPLOADS
    }

    /// Allows another burst of retry frames.
    fn rearm(&mut self) {
        self.failed = 0;
    }
}

/// The stretch's motion, frame loop and counters, owned by the presence.
#[derive(Debug)]
pub(super) struct Stretch {
    spring: Spring,
    drive: Drive,
    ticking: bool,
    pacer: frame_rate::Pacer,
    /// Where the hit proxy is, kept until the outline crosses its margin.
    proxy: Option<[i32; 4]>,
    mask: MaskState,
    /// Since the stretch last became active.
    active: Counts,
    /// Since launch.
    totals: Counts,
    inset: f64,
    bulb_radius: f64,
    reach: f64,
    margin: i32,
    /// How far past the dock's slot the outline can reach, in physical pixels.
    extent: i32,
}

impl Stretch {
    pub(super) fn reposition(&mut self) {
        self.spring = Spring::default();
        if !matches!(self.drive, Drive::Fixture(_)) {
            self.drive = Drive::Rest;
        }
    }

    pub(super) fn new(scale: f32) -> Self {
        let physical = |value: f32| f64::from(value * scale);
        Self {
            spring: Spring::default(),
            drive: Drive::Rest,
            ticking: false,
            pacer: frame_rate::Pacer::default(),
            proxy: None,
            mask: MaskState::default(),
            active: Counts::default(),
            totals: Counts::default(),
            inset: physical(PILL_INSET),
            bulb_radius: physical(BULB_RADIUS),
            reach: physical(REACH),
            margin: scaled(MARGIN, scale),
            extent: scaled(REACH + BULB_RADIUS, scale) + 2,
        }
    }

    /// How far past the dock's slot the outline can reach, in physical pixels.
    pub(super) fn extent(&self) -> i32 {
        self.extent
    }

    /// The largest hit proxy any outline in a slot of this size, grown by the extent, can need.
    pub(super) fn mask_size(&self, [width, height]: [i32; 2]) -> [i32; 2] {
        let grow = 2 * (self.extent + self.margin + 1);
        [width + grow, height + grow]
    }

    pub(super) fn totals(&self) -> Counts {
        self.totals
    }
}

impl Presence {
    /// The outline now, in physical screen pixels.
    pub(super) fn outline(&self) -> Outline {
        let inset = self.stretch.inset;
        let [x, y, width, height] = self.layout.dock.map(f64::from);
        let pill = [
            x + inset,
            y + inset,
            width - 2.0 * inset,
            height - 2.0 * inset,
        ];
        let [center_x, center_y] = center(pill);
        let [offset_x, offset_y] = self.stretch.spring.offset;
        Outline {
            pill,
            bulb: [center_x + offset_x, center_y + offset_y],
            bulb_radius: self.stretch.bulb_radius,
        }
    }
    /// What the hit mask owns now, as Windows accepted it: the pixels that take the pointer.
    pub(super) fn owned(&self) -> Ownership {
        self.stretch.mask.owned
    }

    /// Uploads `proposal` to the hit mask. Once Windows accepts it, it's what the mask owns and
    /// its current outline is what the dock draws, repainted only if a pixel changed; a failed
    /// upload advances neither, and the mask resends everything on the next call. Returns
    /// whether the upload was accepted.
    pub(super) fn reshape(&mut self, proposal: Ownership, cx: &mut Context<'_, Self>) -> bool {
        let window = outline::retained(self.stretch.proxy, proposal.bounds(), self.stretch.margin);
        let begin_qpc = observer::begin(cx);
        let update = self.native.reshape(window, |y| proposal.row_spans(y));
        let observed = observer::commit(cx, begin_qpc, &proposal, window, &update);
        self.stretch.active.record(&update);
        self.stretch.totals.record(&update);
        let before = self.stretch.mask.owned;
        self.stretch.mask.record(proposal, &update);
        match &update {
            Ok(MaskUpdate::Shape | MaskUpdate::Moved) => self.stretch.proxy = Some(window),
            Ok(MaskUpdate::Unchanged) => {}
            Err(error) => {
                self.stretch.proxy = None;
                if self.stretch.totals.failed == 1 {
                    tracing::warn!(%error, "can't update the hit mask");
                }
            }
        }
        let after = self.stretch.mask.owned;
        // The mask can change under a still pointer, which Windows may never report.
        let crossed = before != after
            && platform::cursor_position()
                .is_ok_and(|point| before.contains(point) != after.contains(point));
        // The origin follows the dock window, which may have moved even when the mask didn't.
        let origin = [self.layout.overlay[0], self.layout.overlay[1]];
        let drawn = after.current;
        self.dock.update(cx, |dock, cx| {
            if dock.origin != origin || !dock.outline.same_pixels(&drawn) {
                dock.outline = drawn;
                dock.origin = origin;
                dock.drawn = observed;
                cx.notify();
            }
        });
        if crossed && let Err(error) = self.pointer_changed(cx) {
            tracing::debug!(%error, "can't recheck hover after the mask changed");
        }
        update.is_ok()
    }

    /// Pointer events start the stretch on contact and wake a settled stretch on movement.
    pub(super) fn follow_pointer(&mut self, cx: &mut Context<'_, Self>) {
        match self.stretch.drive {
            Drive::Fixture(_) => {}
            Drive::Cursor => self.start_ticking(cx),
            Drive::Rest => {
                if platform::cursor_position().is_ok_and(|point| self.owned().contains(point)) {
                    self.stretch.drive = Drive::Cursor;
                    self.start_ticking(cx);
                } else if self.stretch.mask.failed > 0 {
                    // Retries a failed upload in another bounded burst.
                    self.start_ticking(cx);
                }
            }
        }
    }

    /// `--stretch-fixture`: stretches out and back until Quit, sending no input.
    pub(super) fn start_fixture(&mut self, cx: &mut Context<'_, Self>) {
        self.stretch.drive = Drive::Fixture(Instant::now());
        self.start_ticking(cx);
    }

    /// Starts the frame loop unless it's running. It stops once the bulb settles and the
    /// mask holds its outline.
    pub(super) fn start_ticking(&mut self, cx: &mut Context<'_, Self>) {
        if self.stretch.ticking {
            return;
        }
        self.stretch.ticking = true;
        self.stretch.pacer.start(Instant::now());
        self.stretch.mask.rearm();
        self.stretch.active = Counts::default();
        let source = if matches!(self.stretch.drive, Drive::Fixture(_)) {
            "fixture"
        } else {
            "cursor"
        };
        let [x, y, width, height] = self.outline().pill;
        let [left, top, overlay_width, overlay_height] = self.layout.overlay;
        diagnostics::record(
            cx,
            format_args!(
                "stretch active source={source} pill={x:.0},{y:.0},{width:.0},{height:.0} \
                 overlay={left},{top},{overlay_width},{overlay_height}"
            ),
        );
        let owner = cx.weak_entity();
        if let Err(error) = self
            .dock_window
            .update(cx, |_, window, _| schedule(window, owner))
        {
            self.stretch.ticking = false;
            tracing::warn!(%error, "can't schedule the stretch");
        }
    }

    /// One frame: step the spring, reshape, and ask for another frame unless it settled.
    fn tick(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let now = Instant::now();
        let cap = (self.settings.read(cx).view())
            .map(|view| view.max_frame_rate)
            .unwrap_or_default();
        let Some(step) = (self.stretch.pacer).frame(now, cap, window.refresh_interval(cx)) else {
            schedule(window, cx.weak_entity());
            return;
        };
        let goal = self.goal(now);
        let settled = self.stretch.spring.step(goal, step);
        self.stretch.active.ticks += 1;
        self.stretch.totals.ticks += 1;
        let proposal = self.stretch.mask.owned.then(self.outline());
        let uploaded = self.reshape(proposal, cx);
        let more = match self.stretch.drive {
            // A persistent failure stops frames; the last accepted outline stays shown.
            Drive::Cursor if self.stretch.mask.exhausted() => {
                self.native.watch_pointer();
                false
            }
            _ if self.stretch.mask.exhausted() => false,
            Drive::Fixture(_) => true,
            // A failed upload is retried next frame, and a settled bulb gets one trim frame.
            _ if !settled || !uploaded || !self.stretch.mask.owned.trimmed() => true,
            Drive::Rest => false,
            // Settled under a still pointer: its next move over the body wakes the stretch.
            Drive::Cursor
                if platform::cursor_position().is_ok_and(|point| self.owned().contains(point)) =>
            {
                self.native.watch_pointer();
                false
            }
            Drive::Cursor => {
                self.stretch.drive = Drive::Rest;
                true
            }
        };
        if more {
            schedule(window, cx.weak_entity());
            return;
        }
        self.stretch.ticking = false;
        diagnostics::record(cx, format_args!("stretch settled {}", self.stretch.active));
    }

    /// Where the bulb is headed now, as an offset from the pill's center.
    fn goal(&mut self, now: Instant) -> [f64; 2] {
        let outline = self.outline();
        let room = self.layout.overlay;
        let reach = self.stretch.reach;
        match self.stretch.drive {
            Drive::Rest => [0.0; 2],
            Drive::Fixture(started) => {
                let elapsed = now.duration_since(started).as_secs_f64();
                fixture_goal(&outline, reach, elapsed, room)
            }
            Drive::Cursor => {
                let panel = if self.panel_visibility == PanelVisibility::Visible || self.text_open()
                {
                    PanelVisibility::Visible
                } else {
                    PanelVisibility::Hidden
                };
                let goal = platform::cursor_position()
                    .ok()
                    .filter(|&point| !self.layout.panel_contains(point, panel))
                    .and_then(|point| follow_goal(&outline, point, reach, room));
                goal.unwrap_or_else(|| {
                    self.stretch.drive = Drive::Rest;
                    [0.0; 2]
                })
            }
        }
    }
}

fn schedule(window: &Window, owner: WeakEntity<Presence>) {
    window.on_next_frame(move |window, cx| {
        if owner.update(cx, |this, cx| this.tick(window, cx)).is_err() {
            tracing::debug!("the presence closed during a stretch");
        }
    });
}

fn center([x, y, width, height]: [f64; 4]) -> [f64; 2] {
    [x + width / 2.0, y + height / 2.0]
}

/// The bulb's goal for a pointer at `cursor`, as an offset from the pill's center: under the
/// pointer, or `reach` toward it. `None` once the bulb can't cover the pointer any more.
fn follow_goal(
    outline: &Outline,
    cursor: [i32; 2],
    reach: f64,
    room: [i32; 4],
) -> Option<[f64; 2]> {
    let point = cursor.map(|value| f64::from(value) + 0.5);
    let anchor = outline.anchor(point);
    let (dx, dy) = (point[0] - anchor[0], point[1] - anchor[1]);
    let distance = dx.hypot(dy);
    if distance > reach + outline.bulb_radius {
        return None;
    }
    let along = if distance > reach {
        reach / distance
    } else {
        1.0
    };
    let bulb = [anchor[0] + dx * along, anchor[1] + dy * along];
    Some(offset(outline, bulb, room))
}

/// The fixture's goal `elapsed` seconds in: out and back along the line from the pill toward
/// the middle of its room, or straight down when the pill sits in the middle.
fn fixture_goal(outline: &Outline, reach: f64, elapsed: f64, room: [i32; 4]) -> [f64; 2] {
    let from = center(outline.pill);
    let toward = center(room.map(f64::from));
    let (dx, dy) = (toward[0] - from[0], toward[1] - from[1]);
    let distance = dx.hypot(dy);
    let direction = if distance < 1.0 {
        [0.0, 1.0]
    } else {
        [dx / distance, dy / distance]
    };
    let anchor = outline.anchor([
        from[0] + direction[0] * reach,
        from[1] + direction[1] * reach,
    ]);
    let length = reach * 0.5 * (1.0 - (TAU * elapsed / FIXTURE_PERIOD).cos());
    let bulb = [
        anchor[0] + direction[0] * length,
        anchor[1] + direction[1] * length,
    ];
    offset(outline, bulb, room)
}

/// `bulb` kept wholly inside `room`, as an offset from the pill's center.
fn offset(outline: &Outline, [x, y]: [f64; 2], room: [i32; 4]) -> [f64; 2] {
    let keep = outline.bulb_radius + 1.0;
    let [left, top, width, height] = room.map(f64::from);
    let (low_x, low_y) = (left + keep, top + keep);
    let high_x = (left + width - keep).max(low_x);
    let high_y = (top + height - keep).max(low_y);
    let [center_x, center_y] = center(outline.pill);
    [
        x.clamp(low_x, high_x) - center_x,
        y.clamp(low_y, high_y) - center_y,
    ]
}

#[cfg(test)]
mod tests {
    use super::{MaskState, Spring, fixture_goal, follow_goal};
    use crate::platform::{MaskUpdate, PresenceError};
    use crate::presence::outline::{Outline, Ownership};

    fn close(actual: [f64; 2], expected: [f64; 2]) -> bool {
        (actual[0] - expected[0]).abs() < 1e-9 && (actual[1] - expected[1]).abs() < 1e-9
    }

    /// A 104 x 40 pill at the origin, at rest.
    fn resting() -> Outline {
        Outline {
            pill: [0.0, 0.0, 104.0, 40.0],
            bulb: [52.0, 20.0],
            bulb_radius: 14.0,
        }
    }

    const ROOM: [i32; 4] = [-200, -200, 504, 440];

    #[test]
    fn reposition_resets_cursor_motion_but_preserves_the_fixture_clock() {
        let mut stretch = super::Stretch::new(1.0);
        stretch.drive = super::Drive::Cursor;
        stretch.spring.step([0.0, 90.0], 1.0 / 60.0);
        stretch.reposition();
        assert_eq!(stretch.drive, super::Drive::Rest);
        assert_eq!(stretch.spring, Spring::default());
        let started = std::time::Instant::now();
        stretch.drive = super::Drive::Fixture(started);
        stretch.reposition();
        assert_eq!(stretch.drive, super::Drive::Fixture(started));
    }

    #[test]
    fn the_spring_moves_alike_at_the_slowest_frame_rate() {
        let mut fast = Spring::default();
        let mut slow = Spring::default();
        for _ in 0..4 {
            fast.step([0.0, 90.0], 1.0 / 144.0);
        }
        slow.step([0.0, 90.0], 4.0 / 144.0);
        assert_eq!(slow, fast);
    }

    #[test]
    fn the_spring_settles_on_its_goal_without_overshoot() {
        let mut spring = Spring::default();
        let mut frames = 0;
        while !spring.step([0.0, 90.0], 1.0 / 144.0) {
            assert!(
                spring.offset[1] <= 90.0,
                "critical damping never passes the goal"
            );
            frames += 1;
            assert!(frames < 144, "it settles within a second");
        }
        assert!(
            close(spring.offset, [0.0, 90.0]),
            "on the goal: {:?}",
            spring.offset
        );
        assert!(
            spring.step([0.0, 90.0], 1.0 / 144.0),
            "a settled spring stays settled"
        );
    }

    #[test]
    fn the_bulb_goes_under_the_pointer_within_reach() {
        assert_eq!(
            follow_goal(&resting(), [52, 100], 96.0, ROOM),
            Some([0.5, 80.5])
        );
        assert_eq!(
            follow_goal(&resting(), [10, 20], 96.0, ROOM),
            Some([-41.5, 0.5])
        );
    }

    #[test]
    fn the_bulb_stops_at_its_reach_and_lets_go_beyond_its_radius() {
        let held = follow_goal(&resting(), [52, 125], 96.0, ROOM).expect("still in reach");
        assert!(close(held, [0.5, 96.0]), "held at full reach: {held:?}");
        assert_eq!(follow_goal(&resting(), [52, 140], 96.0, ROOM), None);
    }

    #[test]
    fn the_bulb_stays_inside_its_room() {
        assert_eq!(
            follow_goal(&resting(), [52, 55], 96.0, [0, 0, 104, 60]),
            Some([0.5, 25.0])
        );
    }

    #[test]
    fn the_fixture_goes_out_to_full_reach_and_back() {
        let room = [-200, -20, 504, 300];
        for (elapsed, expected) in [(0.0, [0.0, 0.0]), (1.0, [0.0, 96.0]), (2.0, [0.0, 0.0])] {
            let goal = fixture_goal(&resting(), 96.0, elapsed, room);
            assert!(close(goal, expected), "{elapsed} s in: {goal:?}");
        }
    }

    #[test]
    fn failed_uploads_advance_nothing_and_stop_after_three_until_rearmed() {
        let resting = Ownership::only(resting());
        let stretched = resting.then(Outline {
            bulb: [52.0, 120.0],
            ..super::tests::resting()
        });
        let mut mask = MaskState {
            owned: resting,
            failed: 0,
        };
        for failures in 1..=3 {
            assert!(!mask.exhausted(), "{failures} failures in, frames continue");
            mask.record(stretched, &Err(PresenceError::MaskSize));
            assert_eq!(
                mask.owned, resting,
                "a failed upload owns and draws nothing new"
            );
            assert_eq!(mask.failed, failures);
        }
        assert!(mask.exhausted(), "three failures in a row stop the frames");
        mask.rearm();
        assert!(
            !mask.exhausted(),
            "a pointer event or position choice allows a new burst"
        );
        assert_eq!(mask.owned, resting);
        mask.record(stretched, &Ok(MaskUpdate::Unchanged));
        assert_eq!(
            mask.owned, stretched,
            "an unchanged mask still advances the drawing"
        );
        assert_eq!(mask.failed, 0);
    }
}
