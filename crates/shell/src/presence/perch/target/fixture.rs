//! The hosts and drags the perch's tests and the carry's tests share.

use crate::platform::host::{HostId, Label, PhysicalRect};
use crate::presence::drag::Generation;
use crate::presence::perch::caption::{Caption, Exclusions, Source, WindowTarget};

pub(crate) const HOST: HostId = HostId {
    window: 1,
    process: 2,
    thread: 3,
};
pub(crate) const OTHER: HostId = HostId {
    window: 9,
    process: 2,
    thread: 3,
};

pub(crate) fn rect(left: i32, top: i32, right: i32, bottom: i32) -> PhysicalRect {
    PhysicalRect {
        left,
        top,
        right,
        bottom,
    }
}

/// An 800 × 600 host at 100 %, `dx` right of x = 100, on a 1920 × 1040 work area.
pub(crate) fn host(id: HostId, dx: i32) -> WindowTarget {
    let mut exclusions = Exclusions::default();
    exclusions.rects[0] = Some((rect(792 + dx, 200, 900 + dx, 231), Source::Inferred));
    WindowTarget {
        id,
        frame: rect(100 + dx, 200, 900 + dx, 800),
        work_area: rect(0, 0, 1920, 1040),
        monitor_dpi: 96,
        caption: Caption {
            rect: rect(100 + dx, 200, 792 + dx, 231),
            source: Source::Inferred,
        },
        exclusions,
        label: Label::new(&[]),
    }
}

pub(crate) fn drag(n: u64) -> Generation {
    Generation::new(n)
}
