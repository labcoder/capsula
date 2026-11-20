use std::fs;
use std::path::Path;

use kumochi_protocol::constants::LIVE_CHATGPT_TEXT_STAMPS_FILE;

use super::{BrainStep, DrawKey, parse, report};
use crate::gate::observation;
use crate::scratch;

const RECEIPT: &str = concat!(
    "ready 17 1 20\n",
    "draw 17 9 11 0 150 100 110 130 140 155\n",
    "draw 17 9 11 1 15 100 120 135 145 160\n",
    "draw 17 9 11 2 23 100 122 136 146 170\n",
    "draw 17 9 11 3 31 100 124 138 147 180\n",
    "final 17 0 0 190\n",
);

#[test]
fn brain_receipt_joins_the_exact_one_call_and_four_keys() {
    let steps = parse(RECEIPT, 17).unwrap();
    assert_eq!(steps.len(), 4);
    assert_eq!(
        steps[0],
        BrainStep {
            key: DrawKey {
                run: 9,
                scene: 11,
                index: 0
            },
            bytes: 150,
            call: 100,
            first: 110,
            close: 130,
            durable: 140,
            pipe: 155,
        }
    );
    assert_eq!(
        steps[3].key,
        DrawKey {
            run: 9,
            scene: 11,
            index: 3
        }
    );
    assert_eq!(steps[3].pipe, 180);
    assert_eq!(
        parse(RECEIPT, 18).unwrap_err().to_string(),
        "invalid brain draw ready stamp"
    );
    assert_eq!(
        parse(&RECEIPT.replace("ready 17 1 20", "ready 17 1 101"), 17)
            .unwrap_err()
            .to_string(),
        "invalid brain draw step identity or clock order"
    );
    assert_eq!(
        parse(&RECEIPT.replace("9 11 2", "9 12 2"), 17)
            .unwrap_err()
            .to_string(),
        "brain draw steps changed owner, call, or clock order"
    );
    assert_eq!(
        parse(&RECEIPT.replace("11 2 23", "11 1 23"), 17)
            .unwrap_err()
            .to_string(),
        "invalid brain draw step identity or clock order"
    );
    assert_eq!(
        parse(&RECEIPT.replace("136 146", "136 132"), 17)
            .unwrap_err()
            .to_string(),
        "invalid brain draw step identity or clock order"
    );
    assert_eq!(
        parse(&RECEIPT.replace("final 17 0 0", "final 17 1 0"), 17)
            .unwrap_err()
            .to_string(),
        "invalid or nonterminal brain draw final stamp"
    );
    assert_eq!(
        parse(RECEIPT.trim_end(), 17).unwrap_err().to_string(),
        "truncated brain draw receipt"
    );
    assert_eq!(
        parse(&format!("{RECEIPT}ready 17 1 191\n"), 17)
            .unwrap_err()
            .to_string(),
        "invalid or nonterminal brain draw final stamp"
    );
}

const TRACE: &str = concat!(
    "startup version=2 pid=42 dock_hwnd=0x1 panel_hwnd=0x2 proxy_hwnd=0x3 scale=1 position=TopCenter work_area=-20,-10,100,100 dock=0,0,4,2 overlay=-5,-5,14,12 panel=0,2,10,10 qpc_frequency=1000 row_cap=512 queue=256 repaint_message=0x8010 marker=0x43505331\n",
    "draw_ready run=9 scene=11 index=0 qpc=156\n",
    "draw_active run=9 scene=11 index=0 qpc=160 segment_ms=800 entrance_ms=30 narrated=0\n",
    "draw_paint run=9 scene=11 index=0 qpc=165\n",
    "draw_ready run=9 scene=11 index=1 qpc=161\n",
    "draw_active run=9 scene=11 index=1 qpc=200 segment_ms=1455 entrance_ms=150 narrated=1\n",
    "draw_paint run=9 scene=11 index=1 qpc=210\n",
    "draw_ready run=9 scene=11 index=2 qpc=171\n",
    "draw_active run=9 scene=11 index=2 qpc=900 segment_ms=800 entrance_ms=0 narrated=0\n",
    "draw_paint run=9 scene=11 index=2 qpc=910\n",
    "draw_ready run=9 scene=11 index=3 qpc=181\n",
    "draw_active run=9 scene=11 index=3 qpc=1200 segment_ms=800 entrance_ms=0 narrated=0\n",
    "draw_paint run=9 scene=11 index=3 qpc=1210\n",
    "close commits=0 frames=0 dropped=0 overflow=0 post_failed=0 unposted=0\n",
    "end lines=14 write_errors=0\n",
);

fn observed(dir: &Path) -> observation::Observation {
    let path = dir.join("observer.log");
    fs::write(&path, TRACE).unwrap();
    observation::read(&path, 42).unwrap()
}

#[test]
fn draw_trial_refuses_to_copy_a_receipt_from_another_brain() {
    let dir = scratch::dir("draw-wrong-brain");
    let source = dir.join("source.stamps");
    fs::write(&source, RECEIPT).unwrap();
    let error = report(&dir, &source, 18, &observed(&dir)).unwrap_err();
    assert_eq!(error.to_string(), "invalid brain draw ready stamp");
    assert!(!dir.join(LIVE_CHATGPT_TEXT_STAMPS_FILE).exists());
    assert!(!dir.join("draw-timing.txt").exists());
}

#[test]
fn draw_trial_does_not_estimate_zero_narration_time() {
    let dir = scratch::dir("draw-zero-narration");
    let source = dir.join("source.stamps");
    fs::write(&source, RECEIPT).unwrap();
    let trace = TRACE
        .replace("segment_ms=800", "segment_ms=0")
        .replace("segment_ms=1455", "segment_ms=0");
    let path = dir.join("observer.log");
    fs::write(&path, trace).unwrap();
    let observed = observation::read(&path, 42).unwrap();
    report(&dir, &source, 17, &observed).unwrap();
    let report = fs::read_to_string(dir.join("draw-timing.txt")).unwrap();
    assert!(report.contains("segment_ms=0 independent_entrance_ms=150 narrated=1"));
    assert!(
        !report.contains("estimated_narration_"),
        "nonblank text with zero narration time must not receive a narration estimate"
    );
    assert!(report.contains("Step 3 has an authored after-delay of 200 ms."));
    assert!(!report.contains("Silent segment floor"));
}

#[test]
fn draw_trial_reads_paint_without_overlay_frames_or_completion() {
    let dir = scratch::dir("draw-paint-only");
    let source = dir.join("source.stamps");
    fs::write(&source, RECEIPT).unwrap();
    let observed = observed(&dir);
    assert!(observed.all_painted());
    assert!(observed.frames.is_empty());
    assert_eq!(
        observed.draw_steps.map(|step| step.paint),
        [Some(165), Some(210), Some(910), Some(1210)]
    );
    report(&dir, &source, 17, &observed).unwrap();
    assert_eq!(
        fs::read_to_string(dir.join(LIVE_CHATGPT_TEXT_STAMPS_FILE)).unwrap(),
        RECEIPT
    );
    let report = fs::read_to_string(dir.join("draw-timing.txt")).unwrap();
    assert!(report.contains("present not matched"));
    assert!(report.contains("Step 3 has an authored after-delay of 200 ms."));
    assert!(report.contains("run=9 scene=11 call_to_first_step_ms=10.000000"));
    let steps: Vec<_> = report
        .lines()
        .filter(|line| line.starts_with("step="))
        .collect();
    assert_eq!(steps.len(), 4);
    assert!(steps[0].contains("accepted_paint_qpc=165"));
    assert!(steps[0].ends_with("call_to_first_paint_ms=65.000000"));
    assert!(steps[1].contains("segment_ms=1455 independent_entrance_ms=150 narrated=1"));
    assert!(steps[2].ends_with(
        "estimated_narration_overlap_ms=540.000000 estimated_narration_share_pct=74.07"
    ));
    assert!(steps[3].contains("segment_ms=800 independent_entrance_ms=0 narrated=0"));
    for (step, expected) in
        steps
            .iter()
            .zip(["55.000000", "90.000000", "788.000000", "1086.000000"])
    {
        assert_eq!(
            step.split_whitespace()
                .find_map(|field| field.strip_prefix("first_step_byte_to_accepted_paint_ms=")),
            Some(expected)
        );
    }
    for step in [steps[1], steps[3]] {
        assert!(!step.contains("estimated_narration_"));
    }
    assert_eq!(report.matches("estimated_narration_overlap_ms=").count(), 1);
    assert!(report.ends_with("painted=4/4\n"));

    let path = dir.join("observer.log");
    for (invalid, expected) in [
        (
            TRACE.replace("narrated=1", "narrated=2"),
            "invalid draw narration bit",
        ),
        (
            TRACE.replace("index=2 qpc=910", "index=2 qpc=899"),
            "duplicate or unordered draw paint stamp",
        ),
    ] {
        fs::write(&path, invalid).unwrap();
        assert_eq!(
            observation::read(&path, 42)
                .unwrap_err()
                .root_cause()
                .to_string(),
            expected
        );
    }
}
