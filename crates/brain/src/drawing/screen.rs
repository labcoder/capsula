//! Screen scenes: the gates a screen `begin` meets, its stamp, and the forms it is stored in.
//! A screen scene's boxes stay in memory; neither stored form holds one.

use kumochi_protocol::message::{RunId, SceneId};
use kumochi_scene::record::Surface;
use kumochi_scene::screen::{admit, stamp, without_boxes};
use kumochi_scene::{Begin, Record, Tokenizer, parse_screen_draw_record};

use super::{CallError, Drawing, Output, Pend, Pending, Wait};
use crate::context::{Geometry, NoCanvas};
use crate::engines::{DRAW, Opaque};

/// Whether `draw` may mark the user's screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScreenMarks {
    Off,
    On,
}

/// Off until the tool offers the screen surface. The coordinator reads it once, so turning the
/// screen marks on is this one line.
pub(crate) const SCREEN_MARKS: ScreenMarks = ScreenMarks::Off;

impl Drawing {
    /// What a screen scene of `run` may be drawn on, from the run's picture and route. A run
    /// that was never told parses its records without the screen surface.
    /// A skip's `Changed` stays for the rest of the run: a later announcement of the same
    /// picture never reopens it, and only another withdrawal replaces it.
    pub(crate) fn canvas(&mut self, run: RunId, canvas: Result<Geometry, NoCanvas>) {
        if run <= self.closed {
            return;
        }
        let ledger = self.ledger(run);
        if canvas.is_err() || ledger.canvas != Some(Err(NoCanvas::Changed)) {
            ledger.canvas = Some(canvas);
        }
    }

    /// A call's screen `begin`, stamped with its run's picture size and display before a lookup
    /// or a continuation compares it. Refused with a fixed reason when the run sent no picture of
    /// a display that can be marked.
    pub(super) fn stamped(&mut self, at: usize, mut begin: Begin) -> Result<Begin, CallError> {
        let run = self.calls[at].run;
        let canvas = self
            .runs
            .iter()
            .find(|ledger| ledger.run == run)
            .and_then(|ledger| ledger.canvas);
        let geometry = match canvas {
            Some(Ok(geometry)) => geometry,
            Some(Err(NoCanvas::OtherDisplay)) => return Err(CallError::OtherDisplay),
            Some(Err(NoCanvas::Off)) => return Err(CallError::ScreenOff),
            Some(Err(NoCanvas::Changed)) => return Err(CallError::ScreenChanged),
            Some(Err(NoCanvas::NoPicture)) | None => return Err(CallError::NoScreenPicture),
        };
        self.calls[at].picture = Some(geometry.picture);
        stamp(&mut begin, geometry.picture, geometry.display).map_err(CallError::Step)?;
        Ok(begin)
    }

    /// Sharing the screen was turned off: every screen scene leaves, every call drawing one ends
    /// as `ScreenOff`, and a later screen `begin` is refused the same way. A step of theirs still
    /// being laid out or written settles without reaching the shell.
    pub(super) fn screen_off(&mut self, out: &mut Output) {
        for ledger in &mut self.runs {
            if ledger.canvas.is_some() {
                ledger.canvas = Some(Err(NoCanvas::Off));
            }
        }
        let calls: Vec<_> = self
            .calls
            .iter()
            .filter(|call| !call.done && call.picture.is_some())
            .map(|call| (call.run, call.id.clone()))
            .collect();
        if let Some(Pending {
            wait: Wait::Step(write),
            ..
        }) = &mut self.pending
            && matches!(write.scene, Pend::New { .. })
            && calls
                .iter()
                .any(|(run, call)| write.run == *run && write.call == *call)
        {
            write.clear_requested = true;
        }
        for (run, call) in calls {
            self.end_early(run, &call, CallError::ScreenOff, out);
        }
        let screens: Vec<SceneId> = self
            .scenes
            .iter()
            .filter(|held| !held.gone && held.state.header().surface == Surface::Screen)
            .map(|held| held.scene)
            .collect();
        self.clear_scenes(&screens, out);
    }

    /// The shell skipped screen scene `scene` because the screen may have changed since its
    /// picture: its open calls end as `ScreenChanged` the way `screen_off` ends them, after a
    /// step still being written settles, it leaves, and the runs that drew it or were drawing in
    /// it can't begin another screen scene, which the shell would skip too. Any other scene is
    /// left alone.
    pub(super) fn scene_skipped(&mut self, scene: SceneId, out: &mut Output) {
        let Some(at) = self
            .live(scene)
            .filter(|&at| self.scenes[at].state.header().surface == Surface::Screen)
        else {
            return;
        };
        let calls: Vec<_> = self
            .open_in(scene)
            .into_iter()
            .map(|at| (self.calls[at].run, self.calls[at].id.clone()))
            .collect();
        let runs: Vec<RunId> = self.scenes[at]
            .committers
            .iter()
            .flatten()
            .copied()
            .chain(calls.iter().map(|(run, _)| *run))
            .collect();
        for ledger in &mut self.runs {
            if runs.contains(&ledger.run) && matches!(ledger.canvas, Some(Ok(_))) {
                ledger.canvas = Some(Err(NoCanvas::Changed));
            }
        }
        self.replacement_failed(scene, out);
        for (run, call) in calls {
            self.end_early(run, &call, CallError::ScreenChanged, out);
        }
        self.clear_scenes(&[scene], out);
    }
}

/// A round's native item as a run that took up a screen hold's picture stores it: a `draw` call
/// naming the screen surface keeps its records with every box target replaced when it is one
/// screen scene the surface admits, and none otherwise; one whose arguments don't read or parse
/// whole keeps none. Any other item is stored as it came.
pub(crate) fn stored_item(item: Opaque) -> Opaque {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(item.bytes()) else {
        return item;
    };
    if value["type"] != "function_call" || value["name"] != DRAW {
        return item;
    }
    let Some(records) = kept(value["arguments"].as_str().unwrap_or_default().as_bytes()) else {
        return item;
    };
    let mut with = |records: Vec<serde_json::Value>| {
        value["arguments"] = serde_json::json!({ "records": records }).to_string().into();
        Opaque::new(serde_json::to_vec(&value).ok()?).ok()
    };
    // shortcut: the last fallback keeps the item as it came, which holds no box only because the
    // Responses parser serialises each item from a `serde_json::Value` before `Opaque::new`, whose
    // only refusal is length, so emptying its arguments shrinks it; recheck for another producer.
    with(records).or_else(|| with(Vec::new())).unwrap_or(item)
}

/// The records a `draw` call's arguments are stored with; `None` when the envelope reads whole,
/// every record parses as the reducer parses it with the screen surface offered, the first is a
/// `begin`, and none names the screen. Arguments that don't read or parse whole keep nothing, text
/// after the records array included, and so does any other envelope that isn't a screen `begin`,
/// steps the screen surface admits and one `end` last; those steps lose their boxes.
fn kept(args: &[u8]) -> Option<Vec<serde_json::Value>> {
    let mut tokenizer = Tokenizer::new();
    let mut input = args;
    let mut records = Vec::new();
    loop {
        let next = tokenizer
            .next(&mut input)
            .map(|record| record.map(|record| parse_screen_draw_record(record.bytes)));
        match next {
            Ok(Some(Ok(record))) => records.push(record),
            Ok(None) if tokenizer.finish().is_ok() => break,
            _ => return Some(Vec::new()),
        }
    }
    let screen = |record: &Record| matches!(record, Record::Begin(begin) if begin.surface == Surface::Screen);
    if matches!(records.first(), Some(Record::Begin(_))) && !records.iter().any(screen) {
        return None;
    }
    let last = records.len().saturating_sub(1);
    let mut kept = Vec::new();
    for (index, record) in records.iter().enumerate() {
        let value = match record {
            Record::Begin(_) if index == 0 && index < last && screen(record) => {
                serde_json::to_value(record)
            }
            Record::Step(step)
                if index > 0 && index < last && admit(step, [f32::INFINITY; 2], 0).is_ok() =>
            {
                serde_json::to_value(Record::Step(without_boxes(step)))
            }
            Record::End(_) if index > 0 && index == last => serde_json::to_value(record),
            _ => return Some(Vec::new()),
        };
        let Ok(value) = value else {
            return Some(Vec::new());
        };
        kept.push(value);
    }
    Some(kept)
}
