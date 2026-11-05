use super::observation::{Commit, Geometry, Row, Startup};
use super::*;
use std::fmt::Write as _;

const CSV_HEADER: &str = "Application,ProcessID,SwapChainAddress,Runtime,SyncInterval,PresentFlags,Dropped,TimeInSeconds,msInPresentAPI,msBetweenPresents,AllowsTearing,PresentMode,msUntilRenderComplete,msUntilDisplayed,msBetweenDisplayChange,msFlipDelay,msUntilRenderStart,msGPUActive,QPCTime\n";

fn directory() -> std::path::PathBuf {
    crate::scratch::dir("receipt-test")
}

#[test]
fn resting_gate_requires_displayed_coverage_after_a_valid_warm_trace() {
    let directory = directory();
    let header = CSV_HEADER;
    let warm = directory.join("warm.csv");
    fs::write(
        &warm,
        format!("{header}fixture.exe,42,0x1,DXGI,1,0,0,0.001,0,0,0,Composed: Flip,0,1,0,0,0,0,2\n"),
    )
    .unwrap();
    let chain = super::super::present::read(&warm, 42)
        .unwrap()
        .warm_swapchain()
        .unwrap();
    assert_eq!(chain, 1);
    let observer = directory.join("observer.log");
    let mut log = String::from(
        "geometry id=1 origin_y=-3 rows=10 trimmed=true spans=-,-,0:4,-,-,-;-,-,0:4,-,-,-;-,-,0:4,-,-,-;0:4,-,0:4,-,-,-;-,-,0:4,-,-,-;-,-,0:4,-,-,-;-,-,0:4,-,-,-;-,-,0:4,-,-,-;-,-,0:4,-,-,-;-,-,0:4,-,-,-\n\
         commit seq=1 begin_qpc=10 qpc=11 result=shape proposal=1 epoch=1 geometry=1 proxy=0,-3,4,10\n\
         startup version=2 pid=42 dock_hwnd=0x1 panel_hwnd=0x2 proxy_hwnd=0x3 scale=1 position=TopCenter work_area=-20,-10,100,100 dock=0,0,4,2 overlay=-5,-5,14,12 panel=0,2,10,10 qpc_frequency=1000 row_cap=512 queue=256 repaint_message=0x8010 marker=0x43505331\n\
         paint frame=1 qpc=12 kind=dock hover=0 drawn_epoch=1 drawn_geometry=1 mask_epoch=1\n\
         drawn frame=1 qpc=14\n\
         click qpc=22 point=1,0 mask_epoch=1 marker=0x43505331\n\
         click qpc=32 point=0,0 mask_epoch=1 marker=0x43505331\n\
         close commits=1 frames=1 dropped=0 overflow=0 post_failed=0 unposted=0\n",
    );
    let lines = log.lines().count();
    writeln!(log, "end lines={lines} write_errors=0").unwrap();
    fs::write(&observer, log).unwrap();
    for (name, dropped, include_edge, expected_error) in [
        ("displayed", 0, true, None),
        (
            "dropped",
            1,
            true,
            Some("center native/display receipt coverage unmeasured"),
        ),
        (
            "missing-edge",
            0,
            false,
            Some("edge native/display receipt coverage unmeasured"),
        ),
    ] {
        let output = directory.join(name);
        fs::create_dir(&output).unwrap();
        let csv = output.join("main.csv");
        fs::write(&csv, format!("{header}fixture.exe,42,0x1,DXGI,1,0,{dropped},0.013,0,0,0,Composed: Flip,0,1,0,0,0,0,13\n")).unwrap();
        let mut trials = format!("{HEADER}\n1,center,1,0,18,19,1,20,21,2,proxy,22,1,1,ok\n");
        if include_edge {
            trials.push_str("2,edge,0,0,28,29,1,30,31,2,proxy,32,1,2,ok\n");
        }
        trials.push_str("3,outside,4,0,38,39,1,40,41,2,edit,42,0,1,ok\n4,external,9,0,48,49,1,50,51,2,edit,52,0,2,ok\n");
        fs::write(output.join("receipt-trials.csv"), trials).unwrap();
        let mut presents = super::super::present::read(&csv, 42).unwrap();
        let result = summarize(&observer, 42, &mut presents, chain, &output, Mode::Rest);
        assert_eq!(
            result.err().map(|error| error.to_string()).as_deref(),
            expected_error,
            "{name}"
        );
        let report = fs::read_to_string(output.join("receipt-joins.txt")).unwrap();
        assert!(report.contains("native_measured="));
        assert!(report.contains("display_measured="));
    }
    fs::remove_dir_all(directory).unwrap();
}

fn sample_data() -> Observation {
    let mut data = Observation::default();
    data.startup = Some(Startup {
        panel_hwnd: 2,
        frequency: 1000,
        work_area: [0, 0, 1000, 1000],
        dock: [0, 0, 100, 40],
        overlay: [0, 0, 200, 40],
        panel: [0, 44, 200, 100],
    });
    for (id, right) in [(1, 120), (2, 140), (3, 130), (4, 110)] {
        data.geometries.insert(
            id,
            Geometry {
                origin_y: 20,
                rows: vec![Row {
                    drawn: [Some([0, right]), None],
                    owned: [Some([0, right.max(130)]), None, None, None],
                }],
            },
        );
    }
    data.commits.push(Commit {
        begin_qpc: 99,
        qpc: 100,
        epoch: 1,
        geometry: 1,
        proxy: [0, 0, 200, 40],
    });
    data
}

#[test]
fn an_undisplayed_present_on_motion_demand_fails_even_with_good_display_gaps() {
    let directory = directory();
    let mut data = sample_data();
    for (qpc, geometry) in [(102, 2), (110, 3), (120, 3)] {
        data.commits.push(Commit {
            begin_qpc: qpc,
            qpc,
            epoch: geometry,
            geometry,
            proxy: [0, 0, 200, 40],
        });
    }
    let displayed = [
        Displayed {
            at: 107,
            painted: 105,
            geometry: 2,
            present: 106,
            frame: 1,
        },
        Displayed {
            at: 112,
            painted: 110,
            geometry: 3,
            present: 111,
            frame: 2,
        },
    ];
    for (index, dropped, expected) in [
        (0, 0, None),
        (
            1,
            1,
            Some("moving-frame demand contains undisplayed presents: 1"),
        ),
    ] {
        let mut csv = CSV_HEADER.to_owned();
        for qpc in [101, 106, 108, 111, 116] {
            let drop = if qpc == 108 { dropped } else { 0 };
            writeln!(
                csv,
                "fixture.exe,42,0x1,DXGI,1,0,{drop},0.1,0,0,0,Composed: Flip,0,1,0,0,0,0,{qpc}"
            )
            .unwrap();
        }
        let path = directory.join(format!("{index}.csv"));
        fs::write(&path, csv).unwrap();
        let rows = super::super::present::read(&path, 42).unwrap();
        let mut report = File::create_new(directory.join(format!("{index}.txt"))).unwrap();
        assert_eq!(
            motion_frames(&data, &displayed, &[], &rows, 1, 1000, &mut report)
                .err()
                .map(|error| error.to_string())
                .as_deref(),
            expected
        );
    }
    fs::remove_dir_all(directory).unwrap();
}

fn frame(id: u64, geometry: u64, qpc: i64) -> Frame {
    Frame {
        id,
        geometry,
        paint_qpc: qpc,
        completion_qpc: Some(qpc + 1),
        kind: Kind::Dock,
        hover: 0,
    }
}

#[test]
fn retract_targets_require_recent_contraction_not_an_older_cycle_peak() {
    let mut data = sample_data();
    data.frames = vec![frame(1, 2, 90), frame(2, 3, 95), frame(3, 1, 98)];
    let cover = RECT {
        left: 0,
        top: 0,
        right: 208,
        bottom: 40,
    };
    assert_eq!(
        candidate(&data, "retract", [0, 0, 100, 40], cover),
        Some([139, 20])
    );
    data.frames[1] = frame(2, 4, 95);
    assert_eq!(candidate(&data, "retract", [0, 0, 100, 40], cover), None);
    assert_eq!(
        candidate(&data, "edge", [0, 0, 100, 40], cover),
        Some([0, 20])
    );
    assert_eq!(
        candidate(&data, "previous", [0, 0, 100, 40], cover),
        Some([129, 20])
    );
    assert_eq!(
        candidate(&data, "outside", [0, 0, 100, 40], cover),
        Some([130, 20])
    );
}

#[test]
fn outside_points_pass_through_just_beyond_a_filled_band() {
    // The line draws x 100..170 on rows 21..24; its band owns those columns on rows 11..34, and
    // the proxy is exactly that rectangle, so no unowned pixel lies inside it.
    let mut data = sample_data();
    let rows = (11..34)
        .map(|y| Row {
            drawn: [(21..24).contains(&y).then_some([100, 170]), None],
            owned: [Some([100, 170]), None, None, None],
        })
        .collect();
    data.geometries.insert(9, Geometry { origin_y: 11, rows });
    data.commits.push(Commit {
        begin_qpc: 199,
        qpc: 200,
        epoch: 2,
        geometry: 9,
        proxy: [100, 11, 70, 23],
    });
    data.startup.as_mut().unwrap().panel = [60, 40, 150, 100];
    let dock = [99, 1, 72, 44];
    let cover = cover_rect(dock, [0, 0, 1000, 1000]).unwrap();
    assert_eq!(candidate(&data, "center", dock, cover), Some([135, 23]));
    assert_eq!(candidate(&data, "outside", dock, cover), Some([99, 23]));
    data.startup.as_mut().unwrap().panel = [0, 0, 100, 100];
    assert_eq!(candidate(&data, "outside", dock, cover), Some([170, 23]));
    data.startup.as_mut().unwrap().panel = [0, 15, 1000, 30];
    assert_eq!(candidate(&data, "outside", dock, cover), Some([135, 10]));
    data.startup.as_mut().unwrap().panel = [0, 0, 1000, 34];
    assert_eq!(candidate(&data, "outside", dock, cover), Some([135, 34]));
    data.startup.as_mut().unwrap().panel = [0, 0, 1000, 100];
    assert_eq!(candidate(&data, "outside", dock, cover), None);
}

#[test]
fn receipt_points_never_target_the_status_panel_opened_by_a_body_click() {
    let mut data = sample_data();
    let cover = RECT {
        left: 0,
        top: 0,
        right: 208,
        bottom: 40,
    };
    assert_eq!(
        candidate(&data, "center", [0, 0, 100, 40], cover),
        Some([60, 20])
    );
    data.frames = vec![frame(1, 2, 90), frame(2, 3, 95), frame(3, 1, 98)];
    assert_eq!(
        candidate(&data, "retract", [0, 0, 100, 40], cover),
        Some([139, 20])
    );
    data.startup.as_mut().unwrap().panel = [0, 10, 208, 100];
    for category in [
        "center", "edge", "body", "previous", "outside", "external", "retract",
    ] {
        assert_eq!(candidate(&data, category, [0, 0, 100, 40], cover), None);
    }
}

#[test]
fn newer_unknown_or_overlapping_frames_never_reuse_an_older_display_claim() {
    let mut data = sample_data();
    data.frames = vec![frame(1, 1, 90)];
    let displayed = [Displayed {
        at: 95,
        painted: 90,
        geometry: 1,
        present: 91,
        frame: 1,
    }];
    assert_eq!(
        displayed_body(&data, &displayed, &[], [119, 20], 100, 102),
        Some(true)
    );
    assert_eq!(
        displayed_body(&data, &displayed, &[], [120, 20], 100, 102),
        Some(false)
    );
    let unknown = frame(2, 2, 98);
    assert_eq!(
        displayed_body(&data, &displayed, &[&unknown], [119, 20], 100, 102),
        None
    );
    data.frames.push(Frame {
        completion_qpc: Some(93),
        ..frame(3, 2, 89)
    });
    assert_eq!(
        displayed_body(&data, &displayed, &[], [119, 20], 100, 102),
        None
    );
}

#[test]
fn a_mask_upload_in_flight_at_the_receipt_is_not_a_stable_mask() {
    let mut data = sample_data();
    assert_eq!(stable_mask(&data, 105, 108).unwrap().unwrap().epoch, 1);
    data.commits.push(Commit {
        begin_qpc: 107,
        qpc: 120,
        epoch: 2,
        geometry: 2,
        proxy: [0, 0, 200, 40],
    });
    assert!(stable_mask(&data, 105, 108).unwrap().is_none());
    data.commits[1].begin_qpc = 109;
    assert_eq!(stable_mask(&data, 105, 108).unwrap().unwrap().epoch, 1);
    data.commits[1].begin_qpc = 101;
    data.commits[1].qpc = 103;
    assert_eq!(stable_mask(&data, 105, 108).unwrap().unwrap().epoch, 2);
}

#[test]
fn drawing_demand_starts_only_on_changed_current_pixels_and_keeps_gaps() {
    let mut data = sample_data();
    for (qpc, geometry) in [(101, 1), (102, 2), (103, 3), (104, 3), (105, 4)] {
        data.commits.push(Commit {
            begin_qpc: qpc,
            qpc,
            epoch: geometry,
            geometry,
            proxy: [0, 0, 200, 40],
        });
    }
    assert_eq!(demand_spans(&data).unwrap(), vec![(102, 104), (105, 105)]);
    let padded = Geometry {
        origin_y: 19,
        rows: vec![
            Row {
                drawn: [None, None],
                owned: [Some([0, 140]), None, None, None],
            },
            Row {
                drawn: [Some([0, 120]), None],
                owned: [Some([0, 140]), None, None, None],
            },
        ],
    };
    assert!(
        same_drawn(&data.geometries[&1], &padded),
        "ownership padding isn't drawing demand"
    );
}

#[test]
fn cover_clamps_inside_the_work_area_without_losing_the_overlay() {
    assert_eq!(
        cover_rect([0, 100, 336, 272], [0, 0, 2560, 1400]).unwrap(),
        RECT {
            left: 0,
            top: 92,
            right: 344,
            bottom: 380
        }
    );
    assert!(cover_rect([0, 0, 1100, 100], [0, 0, 2560, 1440]).is_err());
    assert!(inside([3, 4], [3, 4, 2, 2]));
    assert!(!inside([5, 4], [3, 4, 2, 2]));
}
