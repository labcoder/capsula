use super::*;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write as _;

pub(super) const STARTUP: &str = "startup version=2 pid=42 dock_hwnd=0x1 panel_hwnd=0x2 proxy_hwnd=0x3 scale=1 position=TopCenter work_area=-20,-10,100,100 dock=0,0,4,2 overlay=-5,-5,14,12 panel=0,2,10,10 qpc_frequency=1000 row_cap=512 queue=256 repaint_message=0x8010 marker=0x43505331\n";
pub(super) const GEOMETRY: &str =
    "geometry id=1 origin_y=-1 rows=2 trimmed=true spans=0:4,-,0:4,-,-,-;-,-,0:4,-,-,-\n";
pub(super) const COMMIT: &str =
    "commit seq=1 begin_qpc=10 qpc=11 result=shape proposal=1 epoch=1 geometry=1 proxy=0,-1,4,2\n";

pub(super) fn directory() -> std::path::PathBuf {
    crate::scratch::dir("observer-test")
}

pub(super) fn finish(mut text: String, frames: usize, unposted: usize) -> String {
    writeln!(
        text,
        "close commits=1 frames={frames} dropped=0 overflow=0 post_failed=0 unposted={unposted}"
    )
    .unwrap();
    let count = text.lines().count();
    writeln!(text, "end lines={count} write_errors=0").unwrap();
    text
}

fn parsed(text: &str) -> Result<Observation> {
    ensure!(text.ends_with('\n'), "truncated test stream");
    let mut observation = Observation {
        pid: 42,
        ..Observation::default()
    };
    for line in text.lines() {
        observation.line(line)?;
    }
    ensure!(observation.ended, "test stream lacks end");
    ensure!(
        observation.pending_targets.is_empty(),
        "target frame lacks completion"
    );
    Ok(observation)
}

#[test]
fn incremental_reader_buffers_partial_lines_and_retains_bootstrap_frames() {
    let directory = directory();
    let path = directory.join("observer.log");
    let prefix = format!(
        "paint frame=1 qpc=1 kind=dock hover=0 drawn_epoch=0 drawn_geometry=0 mask_epoch=0\n{GEOMETRY}{COMMIT}{STARTUP}paint frame=2 qpc=12 kind=panel hover=0 drawn_epoch=0 drawn_geometry=0 mask_epoch=1\ndrawn frame=1 qpc=13\n"
    );
    let text = finish(prefix, 2, 1);
    fs::write(&path, "geometry id=").unwrap();
    let mut reader = Reader::open(&path, 42).unwrap();
    assert!(
        reader.update().unwrap().geometries.is_empty(),
        "partial line is buffered"
    );
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&GEOMETRY.as_bytes()[12..])
        .unwrap();
    assert_eq!(
        reader.update().unwrap().geometries.len(),
        1,
        "only the appended suffix completes the buffered geometry"
    );
    drop(reader);
    fs::write(&path, &text[..text.len() - 2]).unwrap();
    let mut reader = Reader::open(&path, 42).unwrap();
    assert_eq!(
        reader.update().unwrap().frames.len(),
        2,
        "pre-proxy paints are retained"
    );
    assert!(reader.complete().is_err(), "partial end is not complete");
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&text.as_bytes()[text.len() - 2..])
        .unwrap();
    let observation = reader.update().unwrap();
    let startup = observation.startup.unwrap();
    assert_eq!(
        startup.panel_hwnd, 2,
        "validated Panel handle remains available to exact-PID binding"
    );
    assert_eq!(
        observation.commits[0].begin_qpc, 10,
        "upload begin is preserved"
    );

    let geometry = &observation.geometries[&1];
    assert!(geometry.contains_drawn([0, -1]), "left edge is drawn");
    assert!(!geometry.contains_owned([4, -1]), "right edge is excluded");
    assert!(
        geometry.contains_owned([0, 0]) && !geometry.contains_drawn([0, 0]),
        "the band owns a row the line doesn't draw"
    );
    reader.complete().unwrap();
    assert_eq!(
        reader.update().unwrap().frames.len(),
        2,
        "no duplicate records on empty update"
    );
    assert_eq!(
        read(&path, 42).unwrap().frames[0].geometry,
        0,
        "cold dock may precede its first commit"
    );
    drop(reader);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rejects_unknown_fields_refs_losses_and_invalid_span_unions() {
    let valid = finish(format!("{GEOMETRY}{COMMIT}{STARTUP}"), 0, 0);
    assert!(parsed(&valid).is_ok(), "geometry may precede startup");
    for text in [
        valid.replace("pid=42", "pid=43"),
        valid.replace("dock_hwnd=0x1", "dock_hwnd=0x0"),
        valid.replace("proxy_hwnd=0x3", "proxy_hwnd=0x2"),
        valid.replace("qpc_frequency=1000", "qpc_frequency=0"),
        valid.replace("result=shape", "result=failed"),
        valid.replace("seq=1", "seq=2"),
        valid.replace("begin_qpc=10", "begin_qpc=12"),
        valid.replace("geometry=1", "geometry=2"),
        valid.replace("spans=0:4,-,0:4,-,-,-", "spans=0:4,-,0:3,-,-,-"),
        valid.replace("rows=2", "rows=513"),
        valid.replace("trimmed=true", "trimmed=false"),
        valid.replace("dropped=0", "dropped=1"),
        valid.replace("write_errors=0", "write_errors=1"),
        valid.replace("end lines=4", "end lines=5"),
        valid.replace("scale=1", "scale=1 surprise=2"),
        valid.replace("scale=1", "scale=1 scale=1"),
        format!("{valid}arbitrary text\n"),
    ] {
        assert!(parsed(&text).is_err(), "must reject {text:?}");
    }
    let mut observation = parsed(&valid).unwrap();
    observation.closed = false;
    observation.ended = false;
    observation
        .line(
            "geometry id=2 origin_y=-1 rows=2 trimmed=false spans=8:10,-,8:10,0:4,-,-;-,-,-,-,-,-",
        )
        .unwrap();
    assert!(
        observation.geometries[&2].contains_owned([1, -1]),
        "previous drawn span remains owned"
    );
    assert!(
        !observation.geometries[&2].contains_drawn([1, -1]),
        "old edge is no longer drawn"
    );
    assert!(
        observation
            .line("geometry id=3 origin_y=-1 rows=2 trimmed=false spans=0:2,-,0:2,8:10,-,-;-,-,-,-,-,-")
            .is_ok(),
        "disjoint ownership keeps current/previous order, not coordinate order"
    );
}

#[test]
fn accepts_current_main_band_geometry_only_when_it_owns_exactly_the_proxy() {
    let band = "geometry id=1 origin_y=5 rows=3 trimmed=true spans=-,-,10:14,-,-,-;10:14,-,10:14,-,-,-;-,-,10:14,-,-,-\n";
    let commit = "commit seq=1 begin_qpc=10 qpc=11 result=shape proposal=1 epoch=1 geometry=1 proxy=10,5,4,3\n";
    let valid = finish(format!("{band}{commit}{STARTUP}"), 0, 0);
    let observation = parsed(&valid).unwrap();
    assert_eq!(observation.commits[0].proxy, [10, 5, 4, 3]);
    let geometry = &observation.geometries[&1];
    assert!(geometry.contains_owned([10, 5]) && !geometry.contains_drawn([10, 5]));
    assert!(geometry.contains_drawn([13, 6]));
    for text in [
        valid.replace("proxy=10,5,4,3", "proxy=10,5,4,4"),
        valid.replace("proxy=10,5,4,3", "proxy=10,6,4,3"),
        valid.replace("proxy=10,5,4,3", "proxy=9,5,5,3"),
        valid.replace(";10:14,-,10:14,-,-,-;", ";9:14,-,10:14,-,-,-;"),
        valid.replace(";10:14,-,10:14,-,-,-;", ";10:15,-,10:14,-,-,-;"),
        valid.replace(";10:14,-,10:14,-,-,-;", ";10:14,15:16,10:14,-,-,-;"),
        valid.replace("-,-,10:14,-,-,-;10:14", "-,-,10:13,-,-,-;10:14"),
        valid.replace("-,-,10:14,-,-,-;10:14", "-,-,10:14,0:2,-,-;10:14"),
        valid.replace("-,-,10:14,-,-,-;10:14", "-,-,10:14,-,-,0:2;10:14"),
    ] {
        assert!(parsed(&text).is_err(), "must reject {text:?}");
    }
}

#[test]
fn a_pull_is_an_empty_mask_over_an_area_grown_from_the_dock_and_counts_once_per_run() {
    let line = |id: u32| {
        format!(
            "geometry id={id} origin_y=0 rows=2 trimmed=true spans=-,-,0:4,-,-,-;-,-,0:4,-,-,-\n"
        )
    };
    let empty = |id: u32| format!("geometry id={id} origin_y=0 rows=0 trimmed=true spans=\n");
    let commit = |seq: u32, geometry: u32, proxy: &str| {
        let qpc = 10 * seq;
        format!(
            "commit seq={seq} begin_qpc={qpc} qpc={qpc} result=shape proposal={geometry} epoch={geometry} geometry={geometry} proxy={proxy}\n"
        )
    };
    let stream = |body: String, commits: usize| {
        finish(format!("{STARTUP}{body}"), 0, 0)
            .replace("close commits=1", &format!("close commits={commits}"))
    };
    let pulls = [
        line(1),
        commit(1, 1, "0,0,4,2"),
        empty(2),
        commit(2, 2, "-2,0,8,6"),
        commit(3, 2, "-2,0,8,6"),
        line(3),
        commit(4, 3, "0,0,4,2"),
        empty(4),
        commit(5, 4, "-2,0,8,6"),
    ]
    .concat();
    let observation = parsed(&stream(pulls.clone(), 5)).unwrap();
    assert_eq!(observation.pulls, 2, "each run of pull commits is one pull");
    assert_eq!(observation.commits.len(), 5);
    let path = directory().join("observer.log");
    fs::write(&path, stream(pulls, 5)).unwrap();
    let (loaded, complete) = load(&path, 42);
    assert!(complete.is_ok(), "{complete:?}");
    assert_eq!(loaded.pulls, 2, "the count survives for pulls.txt");
    assert_eq!(
        read(&path, 42).unwrap_err().to_string(),
        "Kumo pull seen by the observer (pulls=2)",
        "every reader, cold start included, refuses a pull"
    );

    let rows = [line(1), commit(1, 1, "-2,0,8,6")].concat();
    assert!(
        parsed(&stream(rows, 1)).is_err(),
        "a mask with rows over the pull area still has to own it exactly"
    );
    for proxy in ["0,0,4,2", "1,0,2,2", "1,0,8,6", "-2,0,8,1"] {
        let text = [
            line(1),
            commit(1, 1, "0,0,4,2"),
            empty(2),
            commit(2, 2, proxy),
        ]
        .concat();
        assert!(
            parsed(&stream(text, 2)).is_err(),
            "an empty mask over {proxy} isn't a pull and owns nothing"
        );
    }
}

#[test]
fn geometry_rows_carry_two_drawn_and_four_owned_spans() {
    let mut observation = parsed(&finish(format!("{GEOMETRY}{COMMIT}{STARTUP}"), 0, 0)).unwrap();
    observation.closed = false;
    observation.ended = false;
    observation
        .line("geometry id=2 origin_y=0 rows=1 trimmed=true spans=1:2,7:8,1:2,4:5,7:8,10:11")
        .unwrap();
    let four = &observation.geometries[&2];
    for x in [1, 4, 7, 10] {
        assert!(four.contains_owned([x, 0]), "span at {x} is owned");
    }
    for x in [0, 2, 6, 9, 11] {
        assert!(!four.contains_owned([x, 0]), "gap at {x} is not owned");
    }
    assert!(four.contains_drawn([1, 0]) && four.contains_drawn([7, 0]));
    assert!(!four.contains_drawn([4, 0]), "owned but not drawn");
    assert!(
        observation
            .line("geometry id=3 origin_y=0 rows=1 trimmed=false spans=1:2,-,1:2,-,-,-")
            .is_err(),
        "the previous body's drawn span must stay owned"
    );
    observation
        .line("geometry id=3 origin_y=0 rows=1 trimmed=false spans=1:2,-,1:2,7:8,-,-")
        .unwrap();
    for (row, why) in [
        (
            "spans=1:2,8:9,1:2,4:5,7:8,10:11",
            "geometry draws outside the band it owns",
        ),
        ("spans=-,-,1:2,4:5,7:8", "invalid geometry row"),
        ("spans=-,-,1:2,4:5,7:8,10:11,13:14", "invalid geometry row"),
    ] {
        let error = observation
            .line(&format!(
                "geometry id=4 origin_y=0 rows=1 trimmed=true {row}"
            ))
            .expect_err(row);
        assert!(format!("{error:#}").contains(why), "{row}: {error:#}");
    }
    let version_1 = finish(
        format!(
            "{GEOMETRY}{COMMIT}{}",
            STARTUP.replace("version=2", "version=1")
        ),
        0,
        0,
    );
    let error = parsed(&version_1).expect_err("version 1 is refused");
    assert!(format!("{error:#}").contains("wrong version"), "{error:#}");
}

#[test]
fn stop_targets_require_actual_panel_bounds_and_completed_matching_frames() {
    let prefix = format!(
        "{GEOMETRY}{COMMIT}hover id=1 qpc=20 visible=true click_qpc=0\npaint frame=1 qpc=21 kind=panel hover=1 drawn_epoch=0 drawn_geometry=0 mask_epoch=1\ntarget frame=1 qpc=22 kind=stop hover=1 rect=1,3,2,3 inside=true\ntarget frame=1 qpc=23 kind=stop hover=1 rect=2,4,2,3 inside=true\n{STARTUP}drawn frame=1 qpc=25\n"
    );
    let valid = finish(prefix.clone(), 1, 0);
    let target = parsed(&valid).unwrap().stop_target.unwrap();
    assert_eq!(
        target,
        StopTarget {
            frame: 1,
            qpc: 23,
            hover: 1,
            rect: [2, 4, 2, 3]
        },
        "only the latest validated target is exposed"
    );
    for text in [
        valid.replace(" click_qpc=0", ""),
        valid.replace("inside=true", "inside=false"),
        valid.replace("kind=stop hover=1", "kind=stop hover=2"),
        valid.replace("target frame=1", "target frame=2"),
        valid.replace("target frame=1 qpc=22", "target frame=1 qpc=20"),
        valid.replace("target frame=1 qpc=23", "target frame=1 qpc=26"),
        valid.replace("rect=1,3,2,3", "rect=9,3,2,3"),
        valid.replace("inside=true", "inside=true unknown=1"),
        finish(prefix.replace("drawn frame=1 qpc=25\n", ""), 1, 1),
    ] {
        assert!(parsed(&text).is_err(), "must reject target stream {text:?}");
    }
}
