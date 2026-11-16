//! Screen scenes: the gates a screen `begin` meets, its stamp, and the forms it is stored in.
//! A screen scene's boxes stay in memory; neither stored form holds one.

use kumochi_protocol::message::{RunId, SceneId};
use kumochi_scene::record::Surface;
use kumochi_scene::screen::{admit, stamp, without_boxes};
use kumochi_scene::{
    Begin, Record, TokenError, Tokenizer, parse_draw_record, parse_screen_draw_record,
};

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
    pub(crate) fn canvas(&mut self, run: RunId, canvas: Result<Geometry, NoCanvas>) {
        if run > self.closed {
            self.ledger(run).canvas = Some(canvas);
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
}

/// A round's native item as a run that took up a screen hold's picture stores it: a `draw` call
/// naming the screen surface keeps its records with every box target replaced when it is one
/// screen scene the surface admits, and none otherwise; one whose records don't read or parse
/// keeps none. Any other item is stored as it came.
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

/// The records a `draw` call's arguments are stored with; `None` when no record names the screen
/// surface. The whole envelope is read before anything is exempt: arguments that don't read or
/// parse whole keep nothing, and so does an envelope naming the screen that isn't one screen
/// scene whose steps the screen surface admits; those steps lose their boxes.
fn kept(args: &[u8]) -> Option<Vec<serde_json::Value>> {
    let mut tokenizer = Tokenizer::new();
    let mut input = args;
    let mut records = Vec::new();
    loop {
        let next = tokenizer
            .next(&mut input)
            .map(|record| record.map(|record| record.bytes.to_vec()));
        match next {
            Ok(Some(bytes)) => records.push(bytes),
            Ok(None) if tokenizer.finish().is_ok() => break,
            Err(TokenError::AfterRecords) => break,
            Ok(None) | Err(_) => return Some(Vec::new()),
        }
    }
    if records.iter().all(|bytes| parse_draw_record(bytes).is_ok()) {
        return None;
    }
    let Ok(records) = records
        .iter()
        .map(|bytes| parse_screen_draw_record(bytes))
        .collect::<Result<Vec<_>, _>>()
    else {
        return Some(Vec::new());
    };
    let screen = |record: &Record| matches!(record, Record::Begin(begin) if begin.surface == Surface::Screen);
    let mut kept = Vec::new();
    for (index, record) in records.iter().enumerate() {
        let value = match record {
            Record::Begin(_) if index == 0 && screen(record) => serde_json::to_value(record),
            Record::Step(step) if index > 0 && admit(step, [f32::INFINITY; 2], 0).is_ok() => {
                serde_json::to_value(Record::Step(without_boxes(step)))
            }
            Record::End(_) if index > 0 => serde_json::to_value(record),
            _ => return Some(Vec::new()),
        };
        let Ok(value) = value else {
            return Some(Vec::new());
        };
        kept.push(value);
    }
    Some(kept)
}
