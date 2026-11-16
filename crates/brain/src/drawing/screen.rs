//! Screen scenes: the gates a screen `begin` meets, its stamp, and the forms it is stored in.
//! A screen scene's boxes stay in memory; neither stored form holds one.

use kumochi_protocol::message::RunId;
use kumochi_scene::record::Surface;
use kumochi_scene::screen::{stamp, without_boxes};
use kumochi_scene::{Begin, Record, TokenError, Tokenizer, parse_screen_draw_record};

use super::{CallError, Drawing};
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
}

/// A round's native item as a run that can draw screen scenes stores it: a `draw` call in a
/// screen scene keeps its records with every box target replaced, and one whose records can't
/// be read far enough to tell its surface, or don't parse, keeps none. Any other item is stored
/// as it came.
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
    // Arguments too short to hold a box are all an item too large with none can have lost.
    with(records).or_else(|| with(Vec::new())).unwrap_or(item)
}

/// The records a `draw` call's arguments are stored with; `None` when its scene isn't a screen
/// scene. A screen scene's steps lose their boxes, and arguments that don't parse as one whole
/// screen call, or never get as far as a `begin`, keep nothing.
fn kept(args: &[u8]) -> Option<Vec<serde_json::Value>> {
    let mut tokenizer = Tokenizer::new();
    let mut input = args;
    let mut records = Vec::new();
    loop {
        let next = tokenizer
            .next(&mut input)
            .map(|record| record.map(|record| record.bytes.to_vec()));
        let bytes = match next {
            Ok(Some(bytes)) => bytes,
            Ok(None) if tokenizer.finish().is_ok() => return Some(records),
            Err(TokenError::AfterRecords) => return Some(records),
            Ok(None) | Err(_) => return Some(Vec::new()),
        };
        let record = match parse_screen_draw_record(&bytes) {
            Ok(Record::Begin(begin)) if records.is_empty() && begin.surface != Surface::Screen => {
                return None;
            }
            Ok(record @ Record::Begin(_)) if records.is_empty() => record,
            Ok(Record::Step(step)) if !records.is_empty() => Record::Step(without_boxes(&step)),
            Ok(record @ (Record::Begin(_) | Record::End(_))) if !records.is_empty() => record,
            _ => return Some(Vec::new()),
        };
        let Ok(value) = serde_json::to_value(&record) else {
            return Some(Vec::new());
        };
        records.push(value);
    }
}
