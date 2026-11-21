//! A fixed offline draw fixture on the same HTTP/SSE adapter as Responses.

use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use kumochi_protocol::message::RunId;
use serde_json::{Value, json, value::RawValue};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{Notify, mpsc};

use super::engine::replay_round;
use crate::connections::model::conn::Connect;
use crate::connections::model::text_observation::{
    DrawStage, Observation, REPLAY_CALL, REPLAY_STEPS,
};
use crate::coordinator::SendKey;
use crate::engines::{Engine, EngineFailure, Finish, Round, RunRequest, SendGate, TurnEvent};
use crate::platform::qpc_now;

pub(crate) const MODEL: &str = "draw-replay";
const RECORDED: &str = include_str!("../../../fixtures/responses/draw_y_squared_1.json");
const STAGED: [&str; 3] = [
    r#"{"step":{"id":"timing-first","say":"one two three four","put":[{"shape":{"id":"timing-a-now","kind":"rect","box":[92,380,14,35],"enter":"none"}},{"shape":{"id":"timing-a","kind":"rect","box":[120,380,35,35],"enter":"none","delay":150}}]}}"#,
    r#"{"step":{"id":"timing-second","put":[{"shape":{"id":"timing-b","kind":"rect","box":[210,380,35,35],"enter":"none"}}]}}"#,
    r#"{"step":{"id":"timing-third","start":{"after":"timing-second","ms":200},"put":[{"shape":{"id":"timing-c","kind":"rect","box":[300,380,35,35],"enter":"none"}}]}}"#,
];
const COMPLETED: &str = "data: {\"type\":\"response.completed\"}\n\n";

pub(crate) struct Replay {
    recipe: Arc<Wire>,
    followup: Option<(Arc<Wire>, Arc<Notify>)>,
    receipt: Arc<Observation>,
    run: Option<OnceLock<RunId>>,
}

impl Replay {
    pub(crate) fn new(receipt: &Arc<Observation>) -> io::Result<Self> {
        Ok(Self {
            recipe: Arc::new(Wire::drawing(Recipe::Plain)?),
            followup: None,
            receipt: Arc::clone(receipt),
            run: Some(OnceLock::new()),
        })
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn narrated() -> io::Result<Self> {
        Self::offline(Recipe::Narrated)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn replacing() -> io::Result<Self> {
        Self::offline(Recipe::Replacement)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn replacement_chain(gate: Arc<Notify>) -> Self {
        let round = |draws: &[(&str, &str, bool)]| {
            let mut sse = String::new();
            for (index, &(id, fill, speech)) in draws.iter().enumerate() {
                let args = json!({"records":[
                    {"begin":{"v":1,"scene":id,"surface":"overlay","alt":"","others":if id == "final" { "clear" } else { "keep" }}},
                    {"step":{
                        "id":id,"start":if speech { "speech" } else { "now" },
                        "say":if speech { Some("fixture speech") } else { None },
                        "put":[{"shape":{"id":id,"kind":"rect","box":[0,0,15,15],"fill":fill,"enter":"none"}}]
                    }},
                    {"end":{}}
                ]});
                append_draw(&mut sse, index, id, &args.to_string());
            }
            sse.push_str(COMPLETED);
            Arc::new(Wire::http(&sse, Vec::new()))
        };
        Self {
            recipe: round(&[
                ("first", "#aa0000", false),
                ("middle", "#00bb00", false),
                ("third", "#0000cc", false),
            ]),
            followup: Some((
                round(&[
                    ("spoken", "#999999", true),
                    ("eviction", "#dddd00", false),
                    ("final", "#00eeee", false),
                ]),
                gate,
            )),
            receipt: Observation::silent_replay(),
            run: Some(OnceLock::new()),
        }
    }

    #[cfg(feature = "test-support")]
    fn offline(recipe: Recipe) -> io::Result<Self> {
        Ok(Self {
            recipe: Arc::new(Wire::drawing(recipe)?),
            followup: None,
            receipt: Observation::silent_replay(),
            run: (recipe != Recipe::Narrated).then(OnceLock::new),
        })
    }
}

impl fmt::Debug for Replay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DrawReplay")
    }
}

impl Engine for Replay {
    fn start(&self, request: RunRequest, events: mpsc::Sender<TurnEvent>, gate: SendGate) -> Round {
        let (wire, hold) = match request.key {
            SendKey::Build(key)
                if key.round == 1
                    && self
                        .run
                        .as_ref()
                        .is_none_or(|run| run.set(request.run).is_ok()) =>
            {
                (Some(Arc::clone(&self.recipe)), None)
            }
            SendKey::Build(key)
                if key.round == 2
                    && self
                        .run
                        .as_ref()
                        .is_none_or(|run| run.get() == Some(&request.run)) =>
            {
                self.followup.as_ref().map_or_else(
                    || (Some(Arc::new(Wire::http(COMPLETED, Vec::new()))), None),
                    |(wire, hold)| (Some(Arc::clone(wire)), Some(Arc::clone(hold))),
                )
            }
            SendKey::Build(key)
                if key.round == 3
                    && self.followup.is_some()
                    && self
                        .run
                        .as_ref()
                        .is_some_and(|run| run.get() == Some(&request.run)) =>
            {
                (Some(Arc::new(Wire::http(COMPLETED, Vec::new()))), None)
            }
            _ => (None, None),
        };
        let receipt = Arc::clone(&self.receipt);
        Box::pin(async move {
            if let Some(hold) = hold {
                hold.notified().await;
            }
            let finish = if let Some(wire) = wire {
                let connect = ReplayConnect {
                    wire,
                    receipt,
                    run: request.run,
                };
                replay_round(connect, &request, &events, gate).await
            } else {
                Finish::Failed(EngineFailure::Provider)
            };
            events.send(TurnEvent::Finished(finish)).await.ok();
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Recipe {
    Plain,
    #[cfg(feature = "test-support")]
    Narrated,
    #[cfg(feature = "test-support")]
    Replacement,
}

struct Wire {
    bytes: Vec<u8>,
    marks: Vec<(usize, DrawStage<'static>)>,
}

impl Wire {
    fn drawing(recipe: Recipe) -> io::Result<Self> {
        #[derive(serde::Deserialize)]
        struct Records<'a> {
            #[serde(borrow)]
            records: [&'a RawValue; 3],
        }

        let Records {
            records: [begin, recorded, end],
        } = serde_json::from_str(RECORDED)?;
        let item = |arguments: &str| {
            json!({
                "type": "function_call", "id": REPLAY_CALL, "call_id": REPLAY_CALL,
                "name": "draw", "arguments": arguments,
            })
        };
        let mut sse = String::new();
        let mut marks = vec![(b"data: ".len(), DrawStage::Call)];
        append(
            &mut sse,
            &json!({"type": "response.output_item.added", "item": item("")}),
        );
        let mut arguments = String::new();
        let mut delta = |data: &str, step: Option<usize>| {
            let prefix = concat!(
                "data: {\"type\":\"response.function_call_arguments.delta\",",
                "\"item_id\":"
            );
            sse.push_str(prefix);
            sse.push_str(&json!(REPLAY_CALL).to_string());
            sse.push_str(",\"delta\":");
            let encoded = json!(data).to_string();
            if let Some(record) = step {
                marks.push((sse.len() + 1, DrawStage::First(record)));
            }
            sse.push_str(&encoded);
            sse.push_str("}\n\n");
            arguments.push_str(data);
        };
        delta(&format!("{{\"records\":[{},", begin.get()), None);
        #[cfg(feature = "test-support")]
        let narrated = recipe != Recipe::Plain;
        #[cfg(feature = "test-support")]
        let replacing = recipe == Recipe::Replacement;
        #[cfg(not(feature = "test-support"))]
        let Recipe::Plain = recipe;
        #[cfg(not(feature = "test-support"))]
        let narrated = false;
        #[cfg(not(feature = "test-support"))]
        let replacing = false;
        let mut records = [
            (0, recorded.get()),
            (1, STAGED[0]),
            (2, STAGED[1]),
            (3, STAGED[2]),
        ];
        if narrated {
            records.swap(1, 2);
        }
        for (index, data) in records {
            if replacing && index != 1 || narrated && !matches!(index, 1 | 2) {
                continue;
            }
            let mut record: Value = serde_json::from_str(data)?;
            if record["step"]["id"].as_str() != Some(REPLAY_STEPS[index]) {
                return Err(io::Error::other("drawing replay step identifiers changed"));
            }
            if narrated && index == 1 {
                record["step"]["start"] = Value::String("speech".into());
                delta(&record.to_string(), Some(index));
            } else {
                delta(data, Some(index));
            }
            delta(",", None);
        }
        delta(&format!("{}]}}", end.get()), None);
        append(
            &mut sse,
            &json!({
                "type": "response.output_item.done", "output_index": 0, "item": item(&arguments),
            }),
        );
        if replacing {
            append_replacement(&mut sse);
        }
        sse.push_str(COMPLETED);
        Ok(Self::http(&sse, marks))
    }

    fn http(sse: &str, mut marks: Vec<(usize, DrawStage<'static>)>) -> Self {
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            sse.len()
        );
        for (offset, _) in &mut marks {
            *offset += header.len();
        }
        let mut bytes = header.into_bytes();
        bytes.extend_from_slice(sse.as_bytes());
        Self { bytes, marks }
    }
}

fn append_replacement(sse: &mut String) {
    const ARGS: &str = r#"{"records":[{"begin":{"v":1,"scene":"replacement","surface":"overlay","alt":""}},{"step":{"id":"fresh","start":"now","put":[{"shape":{"id":"box","kind":"rect","box":[0,0,15,15],"enter":"none"}}]}},{"end":{}}]}"#;
    append_draw(sse, 1, "fixture-replacement", ARGS);
}

fn append_draw(sse: &mut String, index: usize, id: &str, args: &str) {
    let item = |arguments: &str| {
        json!({
            "type": "function_call", "id": id, "call_id": id, "name": "draw", "arguments": arguments,
        })
    };
    append(
        sse,
        &json!({"type":"response.output_item.added","output_index":index,"item":item("")}),
    );
    append(
        sse,
        &json!({"type":"response.function_call_arguments.delta","item_id":id,"delta":args}),
    );
    append(
        sse,
        &json!({"type":"response.output_item.done","output_index":index,"item":item(args)}),
    );
}

fn append(sse: &mut String, event: &Value) {
    sse.push_str("data: ");
    sse.push_str(&event.to_string());
    sse.push_str("\n\n");
}

struct ReplayConnect {
    wire: Arc<Wire>,
    receipt: Arc<Observation>,
    run: RunId,
}

impl Connect for ReplayConnect {
    type Io = ReplayIo;

    fn connect(&self) -> impl Future<Output = io::Result<Self::Io>> + Send {
        std::future::ready(Ok(ReplayIo {
            wire: Arc::clone(&self.wire),
            receipt: Arc::clone(&self.receipt),
            run: self.run,
            cursor: 0,
            mark: 0,
            flushed: false,
        }))
    }
}

struct ReplayIo {
    wire: Arc<Wire>,
    receipt: Arc<Observation>,
    run: RunId,
    cursor: usize,
    mark: usize,
    flushed: bool,
}

impl AsyncRead for ReplayIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.flushed {
            return Poll::Pending;
        }
        let end = self
            .wire
            .bytes
            .len()
            .min(self.cursor + buf.remaining().min(257));
        buf.put_slice(&self.wire.bytes[self.cursor..end]);
        self.cursor = end;
        let marks = &self.wire.marks[self.mark..];
        let read = marks.partition_point(|(offset, _)| *offset < end);
        if read != 0 {
            let at = qpc_now();
            for &(_, stage) in &marks[..read] {
                self.receipt.mark_draw(self.run, at, stage);
            }
            self.mark += read;
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for ReplayIo {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.flushed = true;
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::{DrawStage, RECORDED, REPLAY_STEPS, Recipe, STAGED, Wire};

    #[test]
    fn staged_step_has_new_pixels_in_the_first_sample() {
        use std::sync::{Arc, atomic::AtomicBool};

        use kumochi_scene::display::WideMeasure;
        use kumochi_scene::layout::{LayoutState, PrepareInput, SurfaceGeometry, prepare};
        use kumochi_scene::theme::Theme;
        use kumochi_scene::{
            Admission, Frame, GlobalBudget, Record, SceneState, SceneView, WireId,
        };
        use serde_json::Value;

        const STAGED_AT: u64 = 1900;
        let recorded: Value = serde_json::from_str(RECORDED).unwrap();
        let bytes = serde_json::to_vec(&recorded["records"][0]).unwrap();
        let Record::Begin(begin) = crate::dispatch::validate_draw_record(&bytes).unwrap() else {
            panic!("recorded scene begins");
        };
        let mut state = SceneState::begin(begin).unwrap();
        let mut budget = GlobalBudget::default();
        let mut view = SceneView::new(&LayoutState::new(&state), 0);
        let mut frame = Frame::new();
        for (now, args) in [
            (0, recorded["records"][1].to_string()),
            (STAGED_AT, STAGED[0].to_owned()),
        ] {
            let Record::Step(step) =
                crate::dispatch::validate_draw_record(args.as_bytes()).unwrap()
            else {
                panic!("typed fixture step");
            };
            let Admission::New(admission) = state.admit(&step, &budget).unwrap() else {
                panic!("new fixture step");
            };
            let committed = state.commit(admission, &mut budget).unwrap();
            let input = PrepareInput::new(
                committed.preparation,
                view.layout().clone(),
                SurfaceGeometry::default(),
                Arc::new(Theme::default()),
            )
            .unwrap();
            let prepared = prepare(input, &mut WideMeasure, &AtomicBool::new(false)).unwrap();
            view.activate(prepared, now, 0, &mut frame).unwrap();
        }
        let visible = |frame: &Frame, id: &str| {
            view.layout()
                .handle(&WireId::try_from(id.to_owned()).unwrap())
                .is_some_and(|handle| {
                    frame
                        .items()
                        .iter()
                        .any(|item| item.handle == handle && item.opacity > 0.0)
                })
        };
        view.frame(STAGED_AT, &mut frame).unwrap();
        assert_eq!(
            (
                visible(&frame, "curve"),
                visible(&frame, "timing-a-now"),
                visible(&frame, "timing-a")
            ),
            (true, true, false),
            "the first sample must show a new mark, not merely the earlier plot",
        );
        view.frame(STAGED_AT + 150, &mut frame).unwrap();
        assert!(
            visible(&frame, "timing-a"),
            "the independent entrance remains at 150 ms"
        );
    }

    #[test]
    fn stamps_name_record_bytes_even_when_the_event_spans_reads() {
        let wire = Wire::drawing(Recipe::Plain).unwrap();
        assert_eq!(wire.marks.len(), 5);
        assert!(matches!(wire.marks[0], (at, DrawStage::Call) if wire.bytes[at] == b'{'));
        for (index, &(start, stage)) in wire.marks[1..].iter().enumerate() {
            assert!(matches!(stage, DrawStage::First(record) if record == index));
            assert_eq!(wire.bytes[start], b'{');
            let end = start
                + wire.bytes[start..]
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .unwrap();
            let decoded: String = serde_json::from_slice(&wire.bytes[start - 1..end - 1]).unwrap();
            let record: serde_json::Value = serde_json::from_str(&decoded).unwrap();
            assert_eq!(record["step"]["id"], REPLAY_STEPS[index]);
            assert!(index != 0 || start / 257 != end / 257);
        }
        let end = wire
            .bytes
            .windows(4)
            .position(|span| span == b"\r\n\r\n")
            .unwrap();
        let mut reader = crate::connections::model::sse::Reader::default();
        let mut round = crate::engines::responses::Round::default();
        for chunk in wire.bytes[end + 4..].chunks(257) {
            reader.feed(chunk, &mut |event| round.event(event)).unwrap();
        }
        let events = round.take();
        let args = events
            .iter()
            .find_map(|event| match event {
                crate::engines::TurnEvent::ToolCallReady { args, .. } => Some(args),
                _ => None,
            })
            .expect("replay maps to one complete draw");
        crate::dispatch::validate_draw(args.as_bytes()).expect("the fixed draw envelope validates");
    }
}
