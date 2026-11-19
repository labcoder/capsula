use super::{BrainStep, DrawKey, Unmatched, endpoint, narration_overlap, narration_share, parse};
use crate::gate::{observation, present};

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
    let steps = parse(RECEIPT, 17, 99).unwrap();
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
        parse(RECEIPT, 18, 99).unwrap_err().to_string(),
        "invalid brain draw ready stamp"
    );
    assert_eq!(
        parse(RECEIPT, 17, 101).unwrap_err().to_string(),
        "invalid brain draw step identity or clock order"
    );
    assert_eq!(
        parse(&RECEIPT.replace("9 11 2", "9 12 2"), 17, 99)
            .unwrap_err()
            .to_string(),
        "brain draw steps changed owner, call, or clock order"
    );
    assert_eq!(
        parse(&RECEIPT.replace("11 2 23", "11 1 23"), 17, 99)
            .unwrap_err()
            .to_string(),
        "invalid brain draw step identity or clock order"
    );
    assert_eq!(
        parse(&RECEIPT.replace("136 146", "136 132"), 17, 99)
            .unwrap_err()
            .to_string(),
        "invalid brain draw step identity or clock order"
    );
    assert_eq!(
        parse(&RECEIPT.replace("final 17 0 0", "final 17 1 0"), 17, 99)
            .unwrap_err()
            .to_string(),
        "invalid or nonterminal brain draw final stamp"
    );
    assert_eq!(
        parse(RECEIPT.trim_end(), 17, 99).unwrap_err().to_string(),
        "truncated brain draw receipt"
    );
    assert_eq!(
        parse(&format!("{RECEIPT}ready 17 1 191\n"), 17, 99)
            .unwrap_err()
            .to_string(),
        "invalid or nonterminal brain draw final stamp"
    );
}

const CSV: &str = "Application,ProcessID,SwapChainAddress,Runtime,SyncInterval,PresentFlags,Dropped,TimeInSeconds,msInPresentAPI,msBetweenPresents,AllowsTearing,PresentMode,msUntilRenderComplete,msUntilDisplayed,msBetweenDisplayChange,msFlipDelay,msUntilRenderStart,msGPUActive,QPCTime\n";
const OVERLAY: &str = concat!(
    "draw_ready run=9 scene=11 index=0 qpc=101\n",
    "draw_active run=9 scene=11 index=0 qpc=105 segment_ms=800 entrance_ms=150\n",
    "paint frame=1 qpc=110 kind=overlay hover=0 drawn_epoch=0 drawn_geometry=0 mask_epoch=0\n",
    "drawn frame=1 qpc=120\n",
    "draw_paint run=9 scene=11 index=0 qpc=110 frame=1 hwnd=0x123\n",
);

fn csv_row(chain: u32, qpc: i64, dropped: u8, delay: &str, display: &str, change: &str) -> String {
    format!(
        "app.exe,17,0x{chain},DXGI,0,0,{dropped},0,0,{delay},0,Flip,0,{display},{change},0,0,0,{qpc}\n"
    )
}

#[test]
fn endpoint_refuses_missing_dropped_ambiguous_and_estimated_presents() {
    let observed = observation::fixture(OVERLAY).unwrap();
    let shell = &observed.draw_steps[0];
    for (rows, expected) in [
        (
            csv_row(10, 115, 0, "0", "3", "0"),
            Unmatched::Present(present::Join::Missing),
        ),
        (
            csv_row(20, 115, 1, "0", "NA", "0"),
            Unmatched::Present(present::Join::Dropped),
        ),
        (
            [
                csv_row(30, 115, 0, "0", "3", "0"),
                csv_row(20, 120, 0, "0", "NA", "0"),
            ]
            .concat(),
            Unmatched::Present(present::Join::Ambiguous),
        ),
        (
            csv_row(20, 140, 0, "25", "3", "25"),
            Unmatched::EstimatedSeed,
        ),
    ] {
        let mut presents = present::parse(&format!("{CSV}{rows}"), 17).unwrap();
        let mut chain = None;
        let result = endpoint(
            &observed,
            shell,
            &mut presents,
            0x10,
            1000,
            &mut chain,
            &mut Vec::new(),
        );
        assert_eq!(result.unwrap_err(), expected);
        assert_eq!(chain, None);
    }
}

#[test]
fn endpoint_demands_confirmed_overlay_and_exact_present_interval() {
    let observed = observation::fixture(OVERLAY).unwrap();
    let shell = &observed.draw_steps[0];
    assert_eq!(shell.ready, Some(101));
    assert_eq!(
        shell.active.map(|active| (active.qpc, active.entrance_ms)),
        Some((105, 150))
    );
    assert_eq!(
        shell
            .paint
            .map(|paint| (paint.qpc, paint.frame, paint.hwnd)),
        Some((110, 1, 0x123))
    );
    let dock_only = csv_row(10, 115, 0, "0", "3", "0");
    let exact = csv_row(20, 120, 0, "0", "NA", "0");
    let mut presents = present::parse(&format!("{CSV}{dock_only}{exact}"), 17).unwrap();
    let mut chain = None;
    let mut frames = Vec::new();
    let matched = endpoint(
        &observed,
        shell,
        &mut presents,
        0x10,
        1000,
        &mut chain,
        &mut frames,
    )
    .unwrap();
    assert_eq!(chain, Some(0x20));
    assert_eq!(matched.present.qpc, 120);
    assert_eq!(matched.source, present::Source::Csv { row: 3 });
    assert_eq!(matched.displayed_qpc, None);
    assert_eq!(
        endpoint(
            &observed,
            shell,
            &mut presents,
            0x10,
            1000,
            &mut Some(0x30),
            &mut Vec::new()
        )
        .unwrap_err(),
        Unmatched::SwapchainChanged,
    );
    assert_eq!(
        endpoint(
            &observed,
            shell,
            &mut presents,
            0x10,
            1000,
            &mut chain,
            &mut Vec::new()
        )
        .unwrap_err(),
        Unmatched::Present(present::Join::Reused),
    );
    assert_eq!(
        endpoint(
            &observed,
            shell,
            &mut presents,
            0x10,
            1000,
            &mut chain,
            &mut frames
        )
        .unwrap_err(),
        Unmatched::ReusedFrame,
    );
}

#[test]
fn overlapping_panel_paint_rejects_even_one_eligible_non_dock_csv_row() {
    let competing =
        "paint frame=2 qpc=120 kind=panel hover=0 drawn_epoch=0 drawn_geometry=0 mask_epoch=0\n";
    let trace = OVERLAY.replace("drawn frame=", &format!("{competing}drawn frame="));
    let observed = observation::fixture(&trace).unwrap();
    let mut presents = present::parse(
        &format!("{CSV}app.exe,17,0x20,DXGI,0,0,0,0,0,0,0,Flip,0,NA,0,0,0,0,120\n"),
        17,
    )
    .unwrap();
    assert_eq!(
        endpoint(
            &observed,
            &observed.draw_steps[0],
            &mut presents,
            0x10,
            1000,
            &mut None,
            &mut Vec::new()
        )
        .unwrap_err(),
        Unmatched::CompetingPanelPaint,
    );
}

#[test]
fn narration_includes_only_the_overlap_after_an_independent_entrance() {
    assert_eq!(narration_share(650, 725), "89.65");
    assert_eq!(narration_share(0, 0), "NA");
    assert_eq!(
        narration_overlap(100, 150, 800, 200, 925, 1000).unwrap(),
        650
    );
    assert_eq!(
        narration_overlap(100, 150, 800, 400, 600, 1000).unwrap(),
        200
    );
    assert_eq!(
        narration_overlap(100, 150, 800, 900, 1100, 1000).unwrap(),
        0
    );
    assert_eq!(
        narration_overlap(100, 900, 800, 200, 1100, 1000).unwrap(),
        0
    );
}
