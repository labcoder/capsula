use std::cell::Cell;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::screen::{CaptureError, ScreenError};
use crate::supervise::{read_request, write_message};

pub(crate) const RECT: PhysicalRect = PhysicalRect {
    origin: [0, 0],
    size: [1, 1],
};
pub(crate) const TARGET: ElementTarget = ElementTarget::Window {
    window: WindowRef(7),
    pid: 8,
};

pub(crate) struct Node {
    pub(crate) name: String,
    pub(crate) password: bool,
    pub(crate) rect: PhysicalRect,
    pub(crate) children: Vec<usize>,
}

pub(crate) struct FakeSource {
    pub(crate) nodes: Vec<Node>,
    pub(crate) error: Option<SourceError>,
    pub(crate) fail_at: Option<usize>,
    pub(crate) reads: Cell<usize>,
}

impl FakeSource {
    pub(crate) fn flat(count: usize) -> Self {
        let mut nodes: Vec<_> = (0..count)
            .map(|_| Node {
                name: String::new(),
                password: false,
                rect: RECT,
                children: Vec::new(),
            })
            .collect();
        nodes[0].children = (1..count).collect();
        Self {
            nodes,
            error: None,
            fail_at: None,
            reads: Cell::new(0),
        }
    }
}

impl ElementSource for FakeSource {
    type Node = usize;
    fn root(&mut self, _: ElementTarget) -> Result<(usize, PhysicalRect, u32), SourceError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        Ok((0, self.nodes[0].rect, 8))
    }
    fn read(&mut self, node: &usize) -> Result<NodeProps, SourceError> {
        if self.fail_at == Some(*node) {
            return Err(SourceError::Fault(-7));
        }
        self.reads.set(self.reads.get() + 1);
        let node = &self.nodes[*node];
        Ok(NodeProps {
            name: node.name.clone(),
            password: node.password,
            control_type: 0,
            rect: node.rect,
        })
    }
    fn first_child(&mut self, node: &usize) -> Result<Option<usize>, SourceError> {
        Ok(self.nodes[*node].children.first().copied())
    }
    fn next_sibling(&mut self, node: &usize) -> Result<Option<usize>, SourceError> {
        Ok(self.nodes.iter().find_map(|parent| {
            parent
                .children
                .windows(2)
                .find(|pair| pair[0] == *node)
                .map(|pair| pair[1])
        }))
    }
}

#[derive(Clone)]
pub(crate) struct FakePlatform(pub(crate) bool);
impl ElementPlatform for FakePlatform {
    fn excluded(&mut self, pid: u32) -> Result<bool, SourceError> {
        Ok(self.0 && pid == 8)
    }
}

pub(crate) fn job(capture: u64, until: Instant) -> WalkJob {
    WalkJob {
        target: TARGET,
        capture: CaptureId(capture),
        display: RECT,
        until,
        masks: Arc::new(Mutex::new(MaskSet::new(CaptureId(capture)))),
    }
}

fn unavailable(capture: CaptureId, why: Unavailable) -> ElementTree {
    ElementTree {
        capture,
        window: PhysicalRect {
            origin: [0; 2],
            size: [0; 2],
        },
        elements: Vec::new(),
        cut: None,
        availability: Availability::Unavailable(why),
    }
}

fn run(source: &mut FakeSource, limits: &ElementLimits) -> ElementTree {
    let now = Instant::now();
    walk(
        source,
        &job(1, now + Duration::from_secs(1)),
        limits,
        &|| now,
        &mut FakePlatform(false),
    )
    .unwrap()
}

#[test]
fn elements_cap_reads_512_and_stops_before_513() {
    let limits = ElementLimits {
        bytes: usize::MAX,
        ..LIMITS
    };
    for (count, cut) in [(512, None), (513, Some(Cut::Elements))] {
        let mut source = FakeSource::flat(count);
        let tree = run(&mut source, &limits);
        assert_eq!(tree.elements.len(), 512);
        assert_eq!(source.reads.get(), 512);
        assert_eq!(tree.cut, cut);
        assert_eq!(
            tree.elements[511].id,
            ElementId {
                capture: CaptureId(1),
                index: 511
            }
        );
        assert_eq!(tree.elements[511].parent, Some(0));
    }
}

#[test]
fn depth_16_is_read_and_17_is_skipped_without_losing_siblings() {
    let limits = ElementLimits {
        bytes: usize::MAX,
        ..LIMITS
    };
    for (count, cut) in [(17, None), (18, Some(Cut::Depth))] {
        let mut source = FakeSource::flat(count + 1);
        source.nodes[0].children = vec![1, count];
        for index in 1..count - 1 {
            source.nodes[index].children = vec![index + 1];
        }
        let tree = run(&mut source, &limits);
        assert_eq!(tree.elements.len(), 18);
        assert_eq!(tree.cut, cut);
        assert_eq!(tree.elements[16].parent, Some(15));
        assert_eq!(tree.elements[17].parent, Some(0));
    }
    let mut source = FakeSource::flat(20);
    source.nodes[0].children = vec![1, 19];
    for index in 1..18 {
        source.nodes[index].children = vec![index + 1];
    }
    source.fail_at = Some(19);
    let tree = run(&mut source, &limits);
    assert_eq!(tree.elements.len(), 17);
    assert_eq!(tree.cut, Some(Cut::Provider));
}

#[test]
fn names_are_cut_at_128_characters_not_bytes() {
    for (name, expected, cut) in [
        ("x".repeat(128), "x".repeat(128), false),
        ("x".repeat(129), "x".repeat(128), true),
        ("é".repeat(129), "é".repeat(128), true),
    ] {
        let mut source = FakeSource::flat(1);
        source.nodes[0].name = name;
        let tree = run(&mut source, &LIMITS);
        assert_eq!(tree.elements[0].name, expected);
        assert_eq!(tree.elements[0].name_cut, cut);
        assert_eq!(tree.cut, None);
    }
}

#[test]
fn json_at_60000_fits_and_another_element_is_cut() {
    // Root: 170 + 112; indices 1..9: 167; 10..99: 168; 100..354: 169.
    assert_eq!(282 + 9 * 167 + 90 * 168 + 255 * 169, 60_000);
    for (count, cut) in [(355, None), (356, Some(Cut::Bytes))] {
        let mut source = FakeSource::flat(count);
        source.nodes[0].name = "a".repeat(112);
        let tree = run(&mut source, &LIMITS);
        assert_eq!(tree.elements.len(), 355);
        assert_eq!(tree.cut, cut);
        assert_eq!(
            tree.elements
                .iter()
                .map(|e| serde_json::to_vec(e).unwrap().len())
                .sum::<usize>(),
            60_000
        );
    }
}

#[test]
fn production_byte_cap_binds_before_the_element_cap() {
    // Without the root's 112 name bytes, 355 elements occupy 59,888 bytes; the next adds 169.
    let tree = run(&mut FakeSource::flat(512), &LIMITS);
    assert_eq!(tree.elements.len(), 355);
    assert_eq!(tree.cut, Some(Cut::Bytes));
    assert_eq!(
        tree.elements
            .iter()
            .map(|e| serde_json::to_vec(e).unwrap().len())
            .sum::<usize>(),
        59_888
    );
}

#[test]
fn root_only_unavailability_and_preorder_geometry_are_exact() {
    let tree = run(&mut FakeSource::flat(1), &LIMITS);
    assert_eq!(tree.elements.len(), 1);
    assert_eq!(tree.elements[0].parent, None);
    assert_eq!(tree.elements[0].screen, RECT);
    assert_eq!(tree.elements[0].in_window, RECT);
    assert_eq!(tree.availability, Availability::Read);
    assert_eq!(tree.cut, None);
    for (error, why) in [
        (SourceError::NoWindow, Unavailable::NoWindow),
        (SourceError::Elevated, Unavailable::Elevated),
        (SourceError::Excluded, Unavailable::Excluded),
    ] {
        let mut source = FakeSource::flat(1);
        source.error = Some(error);
        assert_eq!(run(&mut source, &LIMITS), unavailable(CaptureId(1), why));
        assert_eq!(source.reads.get(), 0);
    }
    let mut source = FakeSource::flat(4);
    source.nodes[0].rect = PhysicalRect {
        origin: [-100, -200],
        size: [200, 300],
    };
    source.nodes[0].children = vec![1, 3];
    source.nodes[1].children = vec![2];
    source.nodes[1].rect = PhysicalRect {
        origin: [-90, -180],
        size: [30, 40],
    };
    let tree = run(&mut source, &LIMITS);
    assert_eq!(
        tree.elements.iter().map(|e| e.parent).collect::<Vec<_>>(),
        [None, Some(0), Some(1), Some(0)]
    );
    assert_eq!(
        tree.elements[1].in_window,
        PhysicalRect {
            origin: [10, 20],
            size: [30, 40]
        }
    );
    assert_eq!(
        tree.elements[1].screen,
        PhysicalRect {
            origin: [-90, -180],
            size: [30, 40]
        }
    );
}

#[test]
fn budget_and_provider_failure_keep_exactly_the_read_prefix() {
    let now = Instant::now();
    let mut source = FakeSource::flat(5);
    let ticks = Cell::new(0);
    let clock = || {
        let tick = ticks.get();
        ticks.set(tick + 1);
        now + Duration::from_millis(tick)
    };
    let tree = walk(
        &mut source,
        &job(1, now + Duration::from_millis(3)),
        &LIMITS,
        &clock,
        &mut FakePlatform(false),
    )
    .unwrap();
    assert_eq!(tree.elements.len(), 3);
    assert_eq!(source.reads.get(), 3);
    assert_eq!(tree.cut, Some(Cut::Budget));
    source.fail_at = Some(2);
    let tree = run(&mut source, &LIMITS);
    assert_eq!(tree.elements.len(), 2);
    assert_eq!(tree.cut, Some(Cut::Provider));
}

fn assert_absent(bytes: &[u8], sentinel: &str) {
    let text = String::from_utf8_lossy(bytes);
    assert!(!text.contains(sentinel), "carrier leaked text");
    assert!(
        !text.contains(&format!("{:?}", sentinel.as_bytes())),
        "carrier leaked byte values"
    );
    assert!(
        !bytes
            .windows(sentinel.len())
            .any(|part| part == sentinel.as_bytes()),
        "carrier leaked bytes"
    );
}

#[test]
fn password_and_descendant_sentinels_never_escape_and_masks_survive_cuts() {
    for stop in [None, Some(Cut::Budget), Some(Cut::Provider)] {
        let now = Instant::now();
        let mut source = FakeSource::flat(4);
        source.nodes[0].name = "plain_name_sentinel".into();
        source.nodes[0].children = vec![1, 3];
        source.nodes[1].name = "password_name_sentinel".into();
        source.nodes[1].password = true;
        source.nodes[1].rect = PhysicalRect {
            origin: [10, 20],
            size: [30, 40],
        };
        source.nodes[1].children = vec![2];
        source.nodes[2].name = "password_child_sentinel".into();
        source.fail_at = (stop == Some(Cut::Provider)).then_some(3);
        let ticks = Cell::new(0);
        let clock = || {
            let tick = ticks.get();
            ticks.set(tick + 1);
            now + Duration::from_millis(tick)
        };
        let job = job(
            1,
            now + Duration::from_millis(if stop == Some(Cut::Budget) { 2 } else { 100 }),
        );
        let tree = walk(&mut source, &job, &LIMITS, &clock, &mut FakePlatform(false)).unwrap();
        assert_eq!(tree.elements.len(), if stop.is_none() { 3 } else { 2 });
        assert_eq!(tree.cut, stop);
        assert_eq!(tree.elements[1].name, "");
        assert!(
            !tree.elements[1].name_cut,
            "password name was truncated rather than removed"
        );
        assert_eq!(
            job.masks.lock().unwrap().rects(),
            [PhysicalRect {
                origin: [10, 20],
                size: [30, 40]
            }]
        );
        assert_eq!(job.masks.lock().unwrap().generation(), 2);
        assert_eq!(
            job.masks.lock().unwrap().completeness(),
            if stop.is_none() {
                Completeness::Complete
            } else {
                Completeness::Partial
            }
        );
        let text = format!(
            "{tree:?} {:?} {:?} {:?} {:?}",
            tree.elements[0],
            tree.elements[1],
            NodeProps {
                name: "plain_name_sentinel".into(),
                password: false,
                control_type: 0,
                rect: RECT
            },
            CaptureError::Screen(ScreenError::Fault(-7))
        );
        for sentinel in [
            "plain_name_sentinel",
            "password_name_sentinel",
            "password_child_sentinel",
        ] {
            assert_absent(text.as_bytes(), sentinel);
        }
        let reply = tree;
        let mut framed = Vec::new();
        write_message(&mut framed, 4, &reply, super::super::wire::MAX_REPLY_BYTES).unwrap();
        let debug = format!("{reply:?}");
        for bytes in [
            serde_json::to_vec(&reply).unwrap(),
            kumochi_protocol::frame::encode_payload(&reply).unwrap(),
            framed.clone(),
            debug.into_bytes(),
        ] {
            for sentinel in ["password_name_sentinel", "password_child_sentinel"] {
                assert_absent(&bytes, sentinel);
            }
        }
        assert_eq!(
            read_request::<ElementTree>(
                &mut framed.as_slice(),
                super::super::wire::MAX_REPLY_BYTES
            )
            .unwrap(),
            Some((4, reply))
        );
    }
}

#[test]
fn root_owner_exclusion_applies_to_foreground_and_window_targets() {
    for target in [
        ElementTarget::Foreground,
        ElementTarget::Window {
            window: WindowRef(7),
            pid: 9,
        },
    ] {
        let now = Instant::now();
        let mut job = job(1, now + Duration::from_secs(1));
        job.target = target;
        let mut source = FakeSource::flat(1);
        assert_eq!(
            walk(&mut source, &job, &LIMITS, &|| now, &mut FakePlatform(true)).unwrap(),
            unavailable(CaptureId(1), Unavailable::Excluded)
        );
        assert_eq!(source.reads.get(), 0);
        assert_eq!(
            job.masks.lock().unwrap().completeness(),
            Completeness::Unknown(Unavailable::Excluded)
        );
    }
}

#[test]
fn empty_password_rectangles_leave_masks_partial() {
    for (size, completeness) in [
        ([0, 0], Completeness::Partial),
        ([0, 1], Completeness::Partial),
        ([1, 0], Completeness::Partial),
        ([1, 1], Completeness::Complete),
    ] {
        let now = Instant::now();
        let job = job(1, now + Duration::from_secs(1));
        let mut source = FakeSource::flat(2);
        source.nodes[1].password = true;
        source.nodes[1].rect = PhysicalRect {
            origin: [10, 20],
            size,
        };
        let tree = walk(
            &mut source,
            &job,
            &LIMITS,
            &|| now,
            &mut FakePlatform(false),
        )
        .unwrap();
        assert_eq!(tree.availability, Availability::Read);
        assert_eq!(tree.cut, None);
        assert_eq!(tree.elements.len(), 2);
        assert_eq!(source.reads.get(), 2);
        let masks = job.masks.lock().unwrap();
        assert_eq!(masks.completeness(), completeness);
        assert_eq!(masks.generation(), 2);
        assert_eq!(
            masks.rects(),
            [PhysicalRect {
                origin: [10, 20],
                size,
            }]
        );
    }
}

#[test]
fn off_display_and_excluded_images_are_unavailable() {
    let now = Instant::now();
    let mut off_display = job(1, now + Duration::from_secs(1));
    let excluded = job(1, now + Duration::from_secs(1));
    let mut source = FakeSource::flat(1);
    off_display.display.origin = [1, 0];
    assert_eq!(
        walk(
            &mut source,
            &off_display,
            &LIMITS,
            &|| now,
            &mut FakePlatform(false)
        )
        .unwrap(),
        unavailable(CaptureId(1), Unavailable::NoWindow)
    );
    assert_eq!(
        walk(
            &mut source,
            &excluded,
            &LIMITS,
            &|| now,
            &mut FakePlatform(true)
        )
        .unwrap(),
        unavailable(CaptureId(1), Unavailable::Excluded)
    );
    assert_eq!(source.reads.get(), 0);
    for (job, why) in [
        (&off_display, Unavailable::NoWindow),
        (&excluded, Unavailable::Excluded),
    ] {
        let masks = job.masks.lock().unwrap();
        assert_eq!(masks.rects(), []);
        assert_eq!(masks.generation(), 1);
        assert_eq!(masks.completeness(), Completeness::Unknown(why));
    }
    for (name, expected) in [
        ("C:\\app\\KUMOCHI.EXE", true),
        ("/app/Kumochid.exe", true),
        ("kumochi.exe.old", false),
        ("another.exe", false),
    ] {
        assert_eq!(excluded_image(name), expected);
    }
}

#[test]
fn element_ids_are_capture_scoped_and_reject_malformed_values() {
    let id = ElementId {
        capture: CaptureId(12),
        index: 7,
    };
    assert_eq!(serde_json::to_string(&id).unwrap(), "\"c12.e7\"");
    assert_eq!(serde_json::from_str::<ElementId>("\"c12.e7\"").unwrap(), id);
    for value in [
        "",
        "12.e7",
        "c12.e65536",
        "c18446744073709551616.e0",
        "c12.e",
        "secret",
    ] {
        assert_eq!(
            serde_json::from_value::<ElementId>(serde_json::Value::String(value.into()))
                .unwrap_err()
                .to_string(),
            "invalid element id"
        );
    }
    let mut masks = MaskSet::new(CaptureId(12));
    masks.push(RECT);
    masks.finish(Completeness::Complete);
    assert_eq!(masks.generation(), 2);
    assert_eq!(masks.capture(), CaptureId(12));
    assert_eq!(masks.completeness(), Completeness::Complete);
    assert_eq!(masks.rects(), [RECT]);
}
