//! Complete 2D draw preflight before any scene record can reach admission.

use std::fmt;

use kumochi_scene::{StepError, TokenError, Tokenizer, parse_draw_record};
use sha2::{Digest as _, Sha256};

use super::{Prepared, Tool};
use crate::drawing::CallError;
use crate::engines::ToolName;

pub(super) fn stream(
    ledger: &mut super::Ledger,
    caller: super::Caller,
    action: super::DrawAction,
) -> super::Step {
    let key = caller.key();
    match &action {
        super::DrawAction::Start if !ledger.start_draw(&key, caller) => return super::Step::Join,
        super::DrawAction::Record { bytes, .. } if record(bytes).is_err() => {
            return super::Step::Answer(super::ToolOutput::Error(super::ToolError::Invalid));
        }
        super::DrawAction::Complete(_) => {
            return super::Step::Answer(super::ToolOutput::Error(super::ToolError::Invalid));
        }
        _ => {}
    }
    super::Step::Screen { key, draw: action }
}

pub(super) const DRAW: Tool = Tool {
    name: ToolName::Draw,
    wire: "draw",
    description: include_str!("../../prompts/draw.md"),
    kumo_only: false,
    mcp: true,
    parameters,
    prepare: |args| Ok(Prepared::Draw(validate(args))),
};

fn parameters() -> serde_json::Value {
    let mut schema = kumochi_scene::draw_schema();
    schema["$defs"]["Surface"] = serde_json::json!({
        "type": "string",
        "const": "overlay",
    });
    schema
}

pub(crate) struct DrawArgs {
    pub(crate) hash: [u8; 32],
    pub(crate) args: Vec<u8>,
}

impl fmt::Debug for DrawArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DrawArgs")
            .field("bytes", &self.args.len())
            .finish_non_exhaustive()
    }
}

pub(crate) fn validate(args: &[u8]) -> Result<DrawArgs, CallError> {
    let mut tokenizer = Tokenizer::new();
    let mut rest = args;
    let mut records = Vec::new();
    loop {
        match tokenizer.next(&mut rest) {
            Ok(Some(record)) => records.push(self::record(record.bytes)?),
            Ok(None) => break tokenizer.finish().map_err(CallError::Token)?,
            Err(TokenError::AfterRecords) => break,
            Err(error) => return Err(CallError::Token(error)),
        }
    }
    let canonical = serde_json::to_vec(&records)
        .map_err(|_| CallError::Step(StepError::new(kumochi_scene::ErrorCode::Malformed)))?;
    let mut hash = Sha256::new();
    hash.update(b"draw\0");
    hash.update(&canonical);
    Ok(DrawArgs {
        hash: hash.finalize().into(),
        args: args.to_vec(),
    })
}

pub(crate) fn record(bytes: &[u8]) -> Result<kumochi_scene::Record, CallError> {
    parse_draw_record(bytes).map_err(|mut error| {
        if error.step.is_none() {
            error.step = serde_json::from_slice::<Named>(bytes)
                .ok()
                .map(|named| named.step.id);
        }
        CallError::Step(error)
    })
}

#[derive(serde::Deserialize)]
struct Named {
    step: NamedStep,
}

#[derive(serde::Deserialize)]
struct NamedStep {
    id: kumochi_scene::WireId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::{Caller, Ledger, RunCtx, Step, call};
    use crate::engines::{CallId, ToolName};
    use kumochi_protocol::message::{RunId, ThreadId};
    use serde_json::json;
    use std::hint::black_box;
    use std::time::Instant;

    #[test]
    fn the_outer_screen_action_never_formats_argument_or_identifier_content() {
        let marker = "zqxdrawprivacysentinel";
        let args = serde_json::to_vec(&json!({"records":[
            {"begin":{"v":1,"scene":marker,"surface":"overlay","alt":marker,"title":marker}},
            {"step":{"id":marker,"say":marker}}, {"end":{}}
        ]}))
        .expect("fixture");
        let draw = validate(&args).expect("valid privacy fixture");
        let action = crate::dispatch::Step::Screen {
            key: crate::dispatch::LedgerKey {
                run: kumochi_protocol::message::RunId(1),
                route: crate::dispatch::Route::Model,
                call: marker.into(),
            },
            draw: super::super::DrawAction::Complete(Ok(draw)),
        };
        let shown = format!("{action:?}");
        assert!(!shown.contains(marker));
        assert!(!shown.contains(format!("{:?}", marker.as_bytes()).trim_matches(['[', ']'])));
    }

    #[test]
    fn canonical_arguments_ignore_key_order_whitespace_and_defaults() {
        let a = validate(br#"{"records":[{"step":{"id":"s"}}]}"#).expect("typed record");
        let b = validate(
            br#"{ "records": [ { "step": { "put": [], "id": "s", "set": [], "remove": [] } } ] }"#,
        )
        .expect("same typed record");
        assert_eq!(a.hash, b.hash);
        let changed = validate(br#"{"records":[{"step":{"id":"t"}}]}"#).expect("changed record");
        assert_ne!(a.hash, changed.hash);
    }

    #[test]
    fn preflight_preserves_the_third_steps_first_typed_failure() {
        let args = serde_json::to_vec(&json!({"records":[
            {"step":{"id":"s1"}}, {"step":{"id":"s2"}},
            {"step":{"id":"s3","put":[{"shape":{"id":"n","kind":"rect","box":[0,0,10,10],"typo":"secret"}}]}}
        ]})).expect("fixture");
        let Err(CallError::Step(error)) = validate(&args) else {
            panic!("a typed refusal");
        };
        assert_eq!(
            error.step.as_ref().map(kumochi_scene::WireId::as_str),
            Some("s3")
        );
        assert_eq!(error.code, kumochi_scene::ErrorCode::UnknownField);
        assert_eq!(error.path.as_str(), "put[0].shape");
    }

    #[test]
    fn preflight_excludes_every_three_dimensional_form_even_null_info() {
        for step in [
            json!({"id":"s3","put":[{"view3d":{"id":"v"}}]}),
            json!({"id":"s3","put":[{"solid":{"in":"v","id":"n","shape":"sphere","r":1}}]}),
            json!({"id":"s3","put":[{"anim":{"target":"n","track":"pos3","keys":[]}}]}),
            json!({"id":"s3","put":[{"anim":{"target":"n","track":"rot3","keys":[]}}]}),
            json!({"id":"s3","put":[{"anim":{"target":"n","track":"pos","keys":[[0,[1,2,3]]]}}]}),
            json!({"id":"s3","set":[{"target":"n","info":"solid"}]}),
            json!({"id":"s3","set":[{"target":"n","info":null}]}),
        ] {
            let args = serde_json::to_vec(&json!({"records":[
                {"step":{"id":"s1"}}, {"step":{"id":"s2"}}, {"step":step}
            ]}))
            .expect("fixture");
            let Err(CallError::Step(error)) = validate(&args) else {
                panic!("unsupported 3D");
            };
            assert_eq!(error.code, kumochi_scene::ErrorCode::Unsupported);
            assert_eq!(
                error.step.as_ref().map(kumochi_scene::WireId::as_str),
                Some("s3")
            );
        }
    }

    #[test]
    fn offered_draw_schema_and_record_gate_restrict_surface_to_overlay() {
        let generic = kumochi_scene::draw_schema();
        let generic_surface = serde_json::to_string(&generic["$defs"]["Surface"]).expect("JSON");
        assert!(generic_surface.contains("\"overlay\""));
        assert!(generic_surface.contains("\"inline\""));
        for offered in [
            super::super::model_schemas(crate::context::Origin::Kumo),
            super::super::mcp_schemas(crate::context::Origin::Kumo),
        ] {
            let draw = offered
                .iter()
                .find(|tool| tool.name == "draw")
                .expect("offered draw tool");
            assert_eq!(
                draw.parameters["$defs"]["Begin"]["properties"]["surface"]["$ref"],
                json!("#/$defs/Surface")
            );
            assert_eq!(
                draw.parameters["$defs"]["Surface"],
                json!({"type": "string", "const": "overlay"})
            );
            assert!(
                !serde_json::to_string(&draw.parameters)
                    .expect("JSON")
                    .contains("\"inline\"")
            );
            let mut expected = generic.clone();
            expected["$defs"]["Surface"] = json!({"type": "string", "const": "overlay"});
            assert_eq!(draw.parameters, expected);
        }
        let overlay = br#"{"begin":{"v":1,"scene":"s","surface":"overlay","alt":"a"}}"#.as_slice();
        assert!(record(overlay).is_ok());
        assert!(
            validate(
                br#"{"records":[{"begin":{"v":1,"scene":"s","surface":"overlay","alt":"a"}},{"end":{}}]}"#
            )
            .is_ok()
        );
        for check in [
            record(br#"{"begin":{"v":1,"scene":"s","surface":"inline","alt":"a"}}"#).err(),
            validate(
                br#"{"records":[{"begin":{"v":1,"scene":"s","surface":"inline","alt":"a"}},{"end":{}}]}"#,
            )
            .err(),
        ] {
            let Some(CallError::Step(error)) = check else {
                panic!("unsupported inline surface");
            };
            assert_eq!(error.step, None);
            assert_eq!(error.path.as_str(), "surface");
            assert_eq!(error.code, kumochi_scene::ErrorCode::Unsupported);
            assert_eq!(error.tool_text(), "surface: scene.Unsupported");
        }
    }

    #[test]
    fn closed_envelopes_refuse_empty_truncated_duplicate_and_mistyped_arguments() {
        for args in [
            b"".as_slice(),
            b"[]",
            b"{\"records\":[",
            b"{\"records\":null}",
        ] {
            assert!(matches!(validate(args), Err(CallError::Token(_))));
        }
    }

    #[test]
    #[ignore = "requires an assigned quiet Rez timing turn; builder runs are dry runs only"]
    #[expect(
        clippy::too_many_lines,
        reason = "the bounded timing fixture also initializes required reader ownership"
    )]
    fn measure_complete_draw_intake() {
        fn fixture(count: usize) -> Vec<u8> {
            let put: Vec<_> = (0..count)
                .map(|n| {
                    json!({"shape":{
                        "id":format!("n{n}"),"kind":"rect","box":[0,0,1,1]
                    }})
                })
                .collect();
            serde_json::to_vec(&json!({"records":[
                {"begin":{"v":1,"scene":"measure","surface":"overlay","alt":"shapes"}},
                {"step":{"id":"s1","put":put}}, {"end":{}}
            ]}))
            .expect("fixture")
        }

        #[derive(Clone, Copy, Debug)]
        enum Path {
            Canonical,
            Adapter,
        }

        fn intake(args: &[u8], path: Path) {
            let run = RunId(1);
            let thread = ThreadId(1);
            let call_id = CallId::new("measure").expect("ASCII");
            let args = if matches!(path, Path::Adapter) {
                let mut ledger = Ledger::default();
                let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
                let mut ctx = RunCtx {
                    build: crate::coordinator::BuildKey {
                        run,
                        round: 1,
                        attempt: 1,
                    },
                    end,
                    control: crate::recall::control::Control::new(end),
                    thread,
                    main: thread,
                    fresh: ThreadId(2),
                    reach: crate::context::Origin::Kumo,
                    offered: &[ToolName::Draw],
                    mcp: None,
                    ledger: &mut ledger,
                };
                let Step::Screen {
                    draw: super::super::DrawAction::Complete(Ok(draw)),
                    ..
                } = call(
                    &mut ctx,
                    Caller::Model {
                        run,
                        call: call_id.clone(),
                    },
                    ToolName::Draw,
                    args,
                )
                else {
                    panic!("valid complete draw");
                };
                draw.args
            } else {
                args.to_vec()
            };
            let output = crate::drawing::Drawing::default()
                .handle(crate::drawing::Input::Call {
                    thread,
                    run,
                    call: call_id,
                    args,
                })
                .expect("one new call");
            assert_eq!(output.effects.len(), 1);
            drop(black_box(output));
        }

        let mut count = 1;
        while fixture(count + 1).len() <= 16 * 1024 {
            count += 1;
        }
        let mut bounded = fixture(count);
        bounded.resize(16 * 1024, b' ');
        for (name, args) in [("small", fixture(1)), ("bounded", bounded)] {
            for path in [Path::Canonical, Path::Adapter] {
                for _ in 0..8 {
                    intake(&args, path);
                }
            }
            for sample in 0..12 {
                for path in if sample % 2 == 0 {
                    [
                        Path::Canonical,
                        Path::Adapter,
                        Path::Adapter,
                        Path::Canonical,
                    ]
                } else {
                    [
                        Path::Adapter,
                        Path::Canonical,
                        Path::Canonical,
                        Path::Adapter,
                    ]
                } {
                    let start = Instant::now();
                    for _ in 0..32 {
                        intake(black_box(&args), path);
                    }
                    println!(
                        "case={name} bytes={} sample={sample} path={path:?} calls=32 ns={}",
                        args.len(),
                        start.elapsed().as_nanos()
                    );
                }
            }
        }
    }
}
