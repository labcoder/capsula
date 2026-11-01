use super::*;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) const STARTUP: &str = "startup version=1 pid=42 dock_hwnd=0x1 panel_hwnd=0x2 proxy_hwnd=0x3 scale=1 position=TopCenter work_area=-20,-10,100,100 dock=0,0,4,2 overlay=-5,-5,14,12 panel=0,2,10,10 qpc_frequency=1000 row_cap=512 queue=256 repaint_message=0x8010 marker=0x43505331\n";
pub(super) const GEOMETRY: &str =
    "geometry id=1 origin_y=-1 rows=2 trimmed=true spans=0:4,0:4,-;-,-,-\n";
pub(super) const COMMIT: &str = "commit seq=1 begin_qpc=10 qpc=11 result=shape proposal=1 epoch=1 geometry=1 proxy=-2,-3,10,10\n";

pub(super) fn directory() -> std::path::PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "{}-observer-test-{}-{stamp}",
        capsula_protocol::constants::OBJECT_PREFIX,
        std::process::id()
    ));
    fs::create_dir(&path).unwrap();
    path
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
    assert!(!geometry.contains_owned([0, 0]), "empty row owns nothing");
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
        valid.replace("spans=0:4,0:4,-", "spans=0:4,0:3,-"),
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
        .line("geometry id=2 origin_y=-1 rows=2 trimmed=false spans=8:10,8:10,0:4;-,-,-")
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
            .line("geometry id=3 origin_y=-1 rows=2 trimmed=false spans=0:2,0:2,8:10;-,-,-")
            .is_ok(),
        "disjoint ownership keeps current/previous order, not coordinate order"
    );
}

#[test]
fn stop_targets_require_actual_panel_bounds_and_completed_matching_frames() {
    let prefix = format!(
        "{GEOMETRY}{COMMIT}hover id=1 qpc=20 visible=true\npaint frame=1 qpc=21 kind=panel hover=1 drawn_epoch=0 drawn_geometry=0 mask_epoch=1\ntarget frame=1 qpc=22 kind=stop hover=1 rect=1,3,2,3 inside=true\ntarget frame=1 qpc=23 kind=stop hover=1 rect=2,4,2,3 inside=true\n{STARTUP}drawn frame=1 qpc=25\n"
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
