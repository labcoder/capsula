use super::{Facts, Geometry, PhysicalRect, Source, geometry};

fn rect(left: i32, top: i32, right: i32, bottom: i32) -> PhysicalRect {
    PhysicalRect {
        left,
        top,
        right,
        bottom,
    }
}

fn fallback(
    dpi: [u32; 2],
    bounds: PhysicalRect,
    frame: PhysicalRect,
    work_area: PhysicalRect,
    lengths: [i32; 2],
) -> Facts {
    Facts {
        per_monitor_v2: true,
        captioned: true,
        bounds,
        frame,
        work_area,
        layout_dpi: dpi[0],
        monitor_dpi: dpi[1],
        title_bar: None,
        buttons: None,
        top_inset: lengths[0],
        button_width: lengths[1],
    }
}

fn exclusions(geometry: &Geometry) -> Vec<(PhysicalRect, Source)> {
    geometry
        .exclusions
        .rects
        .iter()
        .flatten()
        .copied()
        .collect()
}

fn assert_exclusions(geometry: &Geometry, expected: &[(PhysicalRect, Source)]) {
    let actual = exclusions(geometry);
    assert_eq!(actual.len(), expected.len(), "{actual:?}");
    for rect in expected {
        assert!(actual.contains(rect), "missing {rect:?} in {actual:?}");
    }
}

#[test]
fn unaware_host_at_200_percent_scales_the_standard_frame() {
    let facts = fallback(
        [96, 192],
        rect(1000, 200, 2600, 1400),
        rect(1014, 200, 2586, 1386),
        rect(0, 0, 3840, 2112),
        [31, 36],
    );
    let geometry = geometry(&facts).expect("a captioned host gets the standard estimate");
    assert_eq!(geometry.caption.rect, rect(1014, 200, 2370, 262));
    assert_eq!(geometry.caption.source, Source::Inferred);
    assert_exclusions(&geometry, &[(rect(2370, 200, 2586, 262), Source::Inferred)]);
}

#[test]
fn system_aware_host_at_150_percent_on_a_negative_monitor_rounds_up() {
    let facts = fallback(
        [96, 144],
        rect(-1800, 100, -600, 1000),
        rect(-1789, 100, -611, 989),
        rect(-2560, 0, 0, 1392),
        [31, 36],
    );
    let geometry = geometry(&facts).expect("a captioned host gets the standard estimate");
    assert_eq!(geometry.caption.rect, rect(-1789, 100, -773, 147));
    assert_eq!(geometry.caption.source, Source::Inferred);
    assert_exclusions(&geometry, &[(rect(-773, 100, -611, 147), Source::Inferred)]);
}

#[test]
fn only_a_point_in_the_caption_and_outside_its_buttons_is_a_drop() {
    let facts = crate::platform::host::HostFacts {
        id: crate::platform::host::HostId {
            window: 1,
            process: 2,
            thread: 3,
        },
        caption: fallback(
            [96, 144],
            rect(-1800, 100, -600, 1000),
            rect(-1789, 100, -611, 989),
            rect(-2560, 0, 0, 1392),
            [31, 36],
        ),
        label: crate::platform::host::Label::new(&[]),
    };
    let caption = |point| super::drop_target(&facts, point).map(|target| target.caption.rect);
    let inside = rect(-1789, 100, -773, 147);
    assert_eq!(
        caption([-1000, 120]),
        Some(inside),
        "the caption's interior"
    );
    assert_eq!(caption([-1789, 100]), Some(inside), "its top-left corner");
    assert_eq!(caption([-774, 146]), Some(inside), "its last pixel");
    assert_eq!(caption([-1000, 500]), None, "the client area");
    assert_eq!(
        caption([-773, 120]),
        None,
        "the right edge, where the buttons start"
    );
    assert_eq!(caption([-1000, 147]), None, "the bottom edge");
    assert_eq!(caption([-700, 120]), None, "a caption button");
    assert_eq!(caption([-1790, 120]), None, "left of the frame");
}

#[test]
fn maximized_per_monitor_host_cuts_the_band_at_the_work_area() {
    let screen = rect(0, 0, 2560, 1392);
    let facts = fallback(
        [144, 144],
        rect(-11, -11, 2571, 1403),
        screen,
        screen,
        [45, 54],
    );
    let geometry = geometry(&facts).expect("a captioned host gets the standard estimate");
    assert_eq!(geometry.caption.rect, rect(0, 0, 2398, 34));
    assert_exclusions(&geometry, &[(rect(2398, 0, 2560, 34), Source::Inferred)]);
}

#[test]
fn fixed_window_at_100_percent_uses_its_own_inset() {
    let window = rect(100, 100, 700, 500);
    let fixed = fallback([96, 96], window, window, rect(0, 0, 1920, 1040), [26, 36]);
    let framed = fallback(
        [96, 96],
        rect(-1800, 100, -600, 1000),
        rect(-1789, 100, -611, 989),
        rect(-2560, 0, 0, 1392),
        [31, 36],
    );
    for (facts, caption, buttons) in [
        (fixed, rect(100, 100, 592, 126), rect(592, 100, 700, 126)),
        (
            framed,
            rect(-1789, 100, -719, 131),
            rect(-719, 100, -611, 131),
        ),
    ] {
        let geometry = geometry(&facts).expect("a captioned host gets the standard estimate");
        assert_eq!(geometry.caption.rect, caption);
        assert_exclusions(&geometry, &[(buttons, Source::Inferred)]);
    }
}

#[test]
fn borderless_host_with_disagreeing_reports_gets_a_guessed_band() {
    let window = rect(200, 150, 1000, 750);
    let facts = Facts {
        captioned: false,
        buttons: Some(rect(700, 0, 800, 30)),
        ..fallback([96, 96], window, window, rect(0, 0, 1920, 1040), [31, 36])
    };
    let geometry = geometry(&facts).expect("revision 12 guesses instead of refusing");
    assert_eq!(geometry.caption.rect, rect(200, 150, 892, 181));
    assert_eq!(geometry.caption.source, Source::Inferred);
    assert_exclusions(
        &geometry,
        &[
            (rect(900, 150, 1000, 180), Source::Reported),
            (rect(892, 150, 1000, 181), Source::Inferred),
        ],
    );
}

#[test]
fn wrong_scale_report_inside_the_title_bar_is_read_both_ways() {
    let window = rect(1000, 200, 2600, 1400);
    let facts = Facts {
        title_bar: Some(rect(1000, 200, 2600, 262)),
        buttons: Some(rect(692, 0, 800, 31)),
        ..fallback([96, 192], window, window, rect(0, 0, 3840, 2112), [31, 36])
    };
    let geometry = geometry(&facts).expect("the reports agree");
    assert_eq!(geometry.caption.rect, rect(1000, 200, 1692, 262));
    assert_eq!(geometry.caption.source, Source::Reported);
    let physical = rect(1692, 200, 1800, 231);
    let host_units = rect(2384, 200, 2600, 262);
    assert_exclusions(
        &geometry,
        &[
            (physical, Source::Reported),
            (host_units, Source::Reported),
            (rect(2384, 200, 2600, 262), Source::Inferred),
        ],
    );
    assert!(
        physical.right <= host_units.left,
        "the physical reading alone misses the real controls"
    );
}

#[test]
fn borderless_guess_at_unequal_dpis_starts_at_the_frame() {
    let facts = Facts {
        captioned: false,
        buttons: Some(rect(800, 0, 700, 31)),
        ..fallback(
            [96, 144],
            rect(-8, 300, 1208, 1100),
            rect(0, 310, 1200, 1092),
            rect(0, 0, 2560, 1392),
            [31, 36],
        )
    };
    let geometry = geometry(&facts).expect("revision 12 guesses instead of refusing");
    assert_eq!(geometry.caption.rect, rect(0, 310, 1038, 357));
    assert_eq!(geometry.caption.source, Source::Inferred);
    assert_exclusions(&geometry, &[(rect(1038, 310, 1200, 357), Source::Inferred)]);
}

#[test]
fn a_reported_button_past_the_strip_stays_excluded() {
    let window = rect(200, 150, 1000, 750);
    let facts = Facts {
        captioned: false,
        buttons: Some(rect(500, 0, 800, 40)),
        ..fallback([96, 96], window, window, rect(0, 0, 1920, 1040), [31, 36])
    };
    let geometry = geometry(&facts).expect("revision 12 guesses instead of refusing");
    assert_eq!(geometry.caption.rect, rect(200, 150, 700, 181));
    assert_exclusions(
        &geometry,
        &[
            (rect(700, 150, 1000, 190), Source::Reported),
            (rect(892, 150, 1000, 181), Source::Inferred),
        ],
    );
}

#[test]
fn thread_that_is_not_per_monitor_v2_fails_closed() {
    let window = rect(100, 100, 700, 500);
    let facts = Facts {
        per_monitor_v2: false,
        ..fallback([96, 96], window, window, rect(0, 0, 1920, 1040), [26, 36])
    };
    assert!(geometry(&facts).is_none());
}

#[test]
fn a_target_never_prints_its_title() {
    let window = rect(100, 100, 700, 500);
    let facts = crate::platform::host::HostFacts {
        id: crate::platform::host::HostId {
            window: 1,
            process: 2,
            thread: 3,
        },
        caption: fallback([96, 96], window, window, rect(0, 0, 1920, 1040), [26, 36]),
        label: crate::platform::host::fixture::sentinel(),
    };
    let target = super::target_of(&facts).expect("a fixed window has a caption");
    assert_eq!(target.label, facts.label);
    let text = format!("{target:?}");
    assert!(text.contains("label: Label(19 units)"), "{text}");
    crate::platform::host::fixture::assert_no_sentinel(&text);
}
