//! The shell side of a carry: reads the app under the pointer, manages its watch and label,
//! and drains the presence queue on release. How it fits: docs/development/kumo.md, "The carry".

use std::time::Instant;

use gpui_kit::{Context, SharedString, Task};
use kumochi_shell_core::presence::carry::{Candidates, Event as CarryEvent, Sample, decide};

use super::drag::{End, Generation};
use super::perch::{WindowTarget, drop_target};
use super::{Presence, diagnostics};
use crate::platform::host::{Label, WatchGen};
use crate::platform::{self, HostWatch, PresenceEvent};
use crate::strings::{Strings, Text};

/// What Presence holds of a carry. It has no `Debug`, since the label is screen text.
#[derive(Default)]
pub(super) struct Carry {
    candidates: Candidates,
    /// The armed generation's watch, and its drop label.
    watch: Option<(WatchGen, HostWatch, SharedString)>,
    /// The trailing read, waiting for `Candidates::due`.
    trailing: Option<Task<()>>,
}

impl Presence {
    /// One pull frame: once the neck has snapped, the window under the pointer is the
    /// candidate. A move the interval held back gets a trailing read at its deadline, so the
    /// label settles under a resting pointer even when no frame follows.
    pub(super) fn follow_carry(&mut self, cx: &mut Context<'_, Self>) {
        let Some(drag) = self.kumo.read(cx).carrying() else {
            return;
        };
        let point = match platform::cursor_position() {
            Ok(point) => point,
            Err(error) => {
                tracing::debug!(%error, "can't read the pointer mid-carry");
                return;
            }
        };
        self.read_carry(drag, point, cx);
        if let Some(due) = self.carry.candidates.due()
            && self.carry.trailing.is_none()
        {
            self.carry.trailing = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(due.saturating_duration_since(Instant::now()))
                    .await;
                let _gone = this.update(cx, |this, cx| {
                    this.carry.trailing = None;
                    this.follow_carry(cx);
                });
            }));
        }
    }

    /// The candidate read for `drag` with the pointer at `point`, and what it asks for.
    fn read_carry(&mut self, drag: Generation, point: [i32; 2], cx: &mut Context<'_, Self>) {
        match self
            .carry
            .candidates
            .sample(drag, point, Instant::now(), query)
        {
            Sample::Same => {}
            Sample::Arm(id, w, title) => {
                self.carry.watch = None;
                match platform::watch_host(id, w, self.events.clone()) {
                    Ok(watch) => {
                        let label = label(cx.global::<Strings>(), &title);
                        self.carry.watch = Some((w, watch, label));
                    }
                    Err(error) => {
                        tracing::debug!(%error, "can't watch the window under the carry");
                        self.carry.candidates.watch_failed(w);
                    }
                }
            }
            Sample::Retitle(title) => {
                let text = label(cx.global::<Strings>(), &title);
                if let Some((.., shown)) = &mut self.carry.watch {
                    *shown = text;
                }
            }
        }
        self.sync_carry(cx);
    }

    /// A watch's `HostChanged`, or an Esc or Quit that ends a drop under way.
    pub(super) fn carry_event(&mut self, event: &PresenceEvent, cx: &mut Context<'_, Self>) {
        if let Some(event) = carry_signal(*event) {
            self.carry.candidates.event(event);
        }
        self.sync_carry(cx);
    }

    /// The drag `drag` ended with `end`. A release after the snap runs the drop decision.
    pub(super) fn carry_ended(&mut self, drag: Generation, end: End, cx: &mut Context<'_, Self>) {
        let at = match end {
            End::Released(at) if self.carry.candidates.carries(drag) => {
                self.kumo.read(cx).screen_point(at)
            }
            _ => None,
        };
        if let Some(at) = at {
            self.carry.candidates.stale();
            self.read_carry(drag, at, cx);
            let from = self
                .dock
                .read(cx)
                .pull
                .as_ref()
                .and_then(|picture| picture.images[2].as_ref())
                .map_or(at, |(quad, _)| [quad.x, quad.y]);
            let placed = decide(
                self,
                drag,
                |this| &mut this.carry.candidates,
                || query(at),
                from,
                |this| this.drain(cx),
            );
            // shortcut: C2b-3's perch takes a published placement; until then every drop
            // returns to the line, so every ending creates nothing.
            diagnostics::record(cx, format_args!("carry drop perch={}", placed.is_some()));
        }
        self.end_carry(cx);
    }

    /// No carry: nothing is watched and no label shows.
    pub(super) fn end_carry(&mut self, cx: &mut Context<'_, Self>) {
        self.carry.trailing = None;
        self.carry.candidates.end();
        self.sync_carry(cx);
    }

    /// Handles every presence event already queued, through the usual handlers.
    fn drain(&mut self, cx: &mut Context<'_, Self>) {
        while let Ok(event) = self.events_in.try_recv() {
            self.carry_event(&event, cx);
            if !matches!(event, PresenceEvent::HostChanged { .. })
                && let Err(error) = self.on_event(event, cx)
            {
                tracing::error!(%error, "presence update failed");
            }
        }
    }

    /// Keeps the watch only for the generation Targeting still watches, and shows its label.
    fn sync_carry(&mut self, cx: &mut Context<'_, Self>) {
        let watching = self.carry.candidates.watching();
        if self
            .carry
            .watch
            .as_ref()
            .is_some_and(|(w, ..)| Some(*w) != watching)
        {
            self.carry.watch = None;
        }
        let label = self.carry.watch.as_ref().map(|(.., label)| label.clone());
        self.dock.update(cx, |dock, cx| {
            if dock.label != label {
                dock.label = label;
                cx.notify();
            }
        });
    }
}

/// The drop target under `point`: its caption, outside the caption buttons.
fn query(point: [i32; 2]) -> Option<WindowTarget> {
    platform::host_at(point)
        .as_ref()
        .and_then(|facts| drop_target(facts, point))
}

/// The drop label for a window titled `title`, or the fixed copy when it has none.
fn label(strings: &Strings, title: &Label) -> SharedString {
    if title.units().is_empty() {
        return strings.get(Text::PerchUntitled).into();
    }
    strings
        .perch_on(&String::from_utf16_lossy(title.units()))
        .into()
}

fn carry_signal(event: PresenceEvent) -> Option<CarryEvent> {
    match event {
        PresenceEvent::Escape | PresenceEvent::Quit => Some(CarryEvent::Cancel),
        PresenceEvent::HostChanged { w, change } => {
            Some(CarryEvent::HostChanged { watch: w, change })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{CarryEvent, Label, PresenceEvent, Strings, carry_signal, label};
    use crate::platform::host::HostChange;
    use kumochi_shell_core::presence::perch::target_fixtures::watch;

    #[test]
    fn escape_and_quit_cancel_the_drop_but_repaint_does_not() {
        assert_eq!(
            carry_signal(PresenceEvent::Escape),
            Some(CarryEvent::Cancel)
        );
        assert_eq!(carry_signal(PresenceEvent::Quit), Some(CarryEvent::Cancel));
        assert_eq!(carry_signal(PresenceEvent::Repaint), None);
        let watch = watch(2);
        let change = HostChange::Gone;
        assert_eq!(
            carry_signal(PresenceEvent::HostChanged { w: watch, change }),
            Some(CarryEvent::HostChanged { watch, change })
        );
    }

    #[test]
    fn the_label_localizes_named_and_untitled_hosts() {
        let strings = Strings::english();
        for (title, expected) in [
            ("Alpha", "Drop on Alpha"),
            ("Beta", "Drop on Beta"),
            ("", "Drop on this app"),
        ] {
            let title = Label::new(&title.encode_utf16().collect::<Vec<_>>());
            assert_eq!(label(&strings, &title).as_ref(), expected);
        }
    }
}
