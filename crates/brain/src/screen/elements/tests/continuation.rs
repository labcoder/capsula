use super::*;

#[test]
fn password_publication_precedes_the_byte_cut() {
    let now = Instant::now();
    let job = job(1, now + Duration::from_secs(1));
    let mut source = FakeSource::flat(355);
    source.nodes[0].name = "x".repeat(114);
    source.nodes[354].password = true;
    let tree = walk(
        &mut source,
        &job,
        &LIMITS,
        &|| now,
        &mut FakePlatform(false),
    )
    .unwrap();
    assert_eq!(tree.elements.len(), 354);
    assert_eq!(tree.cut, Some(Cut::Bytes));
    assert_eq!(source.reads.get(), 355);
    let masks = job.masks.lock().unwrap();
    assert_eq!(masks.rects(), [RECT]);
    assert_eq!(masks.generation(), 2);
    assert_eq!(masks.completeness(), Completeness::Complete);
}

#[test]
fn mask_only_continuation_has_its_own_exact_read_cap() {
    let limits = ElementLimits {
        elements: 1,
        bytes: usize::MAX,
        ..LIMITS
    };
    for (count, expected, rects) in [
        (2_049, Completeness::Complete, vec![RECT]),
        (2_050, Completeness::Partial, vec![]),
    ] {
        let now = Instant::now();
        let job = job(1, now + Duration::from_secs(1));
        let mut source = FakeSource::flat(count);
        source.nodes[count - 1].password = true;
        let tree = walk(
            &mut source,
            &job,
            &limits,
            &|| now,
            &mut FakePlatform(false),
        )
        .unwrap();
        assert_eq!(tree.elements.len(), 1);
        assert_eq!(tree.cut, Some(Cut::Elements));
        assert_eq!(source.reads.get(), 1);
        assert_eq!(source.mask_reads.get(), 2_048);
        let masks = job.masks.lock().unwrap();
        assert_eq!(masks.completeness(), expected);
        assert_eq!(masks.rects(), rects);
    }
}

#[test]
fn continuation_budget_and_provider_failure_keep_published_masks_partial() {
    let limits = ElementLimits {
        elements: 1,
        ..LIMITS
    };
    for provider_failure in [false, true] {
        let now = Instant::now();
        let job = job(1, now + Duration::from_millis(2));
        let ticks = Cell::new(0);
        let clock = || {
            let tick = ticks.replace(ticks.get() + 1);
            now + Duration::from_millis(tick)
        };
        let mut source = FakeSource::flat(4);
        source.nodes[1].password = true;
        source.nodes[3].password = true;
        source.fail_at = provider_failure.then_some(2);
        let tree = walk(
            &mut source,
            &job,
            &limits,
            &|| if provider_failure { now } else { clock() },
            &mut FakePlatform(false),
        )
        .unwrap();
        assert_eq!(tree.elements.len(), 1);
        assert_eq!(tree.cut, Some(Cut::Elements));
        let masks = job.masks.lock().unwrap();
        assert_eq!(masks.rects(), [RECT]);
        assert_eq!(masks.completeness(), Completeness::Partial);
    }
}

#[test]
fn off_display_names_are_absent_from_every_outer_carrier() {
    let now = Instant::now();
    let job = job(1, now + Duration::from_secs(1));
    let mut source = FakeSource::flat(2);
    source.nodes[1].name = "off_display_sentinel".into();
    source.nodes[1].rect.origin = [1, 0];
    let tree = walk(
        &mut source,
        &job,
        &LIMITS,
        &|| now,
        &mut FakePlatform(false),
    )
    .unwrap();
    assert_eq!(tree.elements[1].name, "");
    assert!(!tree.elements[1].name_cut);
    assert_eq!(
        tree.elements[1].screen,
        PhysicalRect {
            origin: [1, 0],
            size: [1, 1]
        }
    );
    let walked = Walked {
        tree,
        retire: false,
    };
    let tagged = (CaptureId(1), Ok::<_, Failure>(&walked.tree));
    assert_absent(
        format!("{walked:?} {tagged:?} {job:?}").as_bytes(),
        "off_display_sentinel",
    );
    let reply = Reply::Elements(KEY, walked.tree);
    assert_absent(format!("{reply:?}").as_bytes(), "off_display_sentinel");
    assert_absent(&serde_json::to_vec(&reply).unwrap(), "off_display_sentinel");
}

#[test]
fn root_fault_and_foreign_or_poisoned_sinks_cannot_be_trusted() {
    let now = Instant::now();
    let mut source = FakeSource::flat(2);
    source.nodes[1].password = true;
    source.error = Some(SourceError::Fault(-7));
    let job = job(1, now + Duration::from_secs(1));
    assert_eq!(
        walk(
            &mut source,
            &job,
            &LIMITS,
            &|| now,
            &mut FakePlatform(false)
        )
        .err(),
        Some(Failure::Fault(-7))
    );
    assert_eq!(
        job.masks.lock().unwrap().completeness(),
        Completeness::Unknown(Unavailable::Busy)
    );
    source.error = None;
    let mut foreign = super::job(1, job.until);
    foreign.masks = Arc::new(Mutex::new(MaskSet::new(CaptureId(2))));
    assert_eq!(
        walk(
            &mut source,
            &foreign,
            &LIMITS,
            &|| now,
            &mut FakePlatform(false)
        )
        .unwrap(),
        unavailable(CaptureId(1), Unavailable::Busy)
    );
    assert_eq!(
        foreign.masks.lock().unwrap().completeness(),
        Completeness::Unknown(Unavailable::Busy)
    );
    let poisoned = super::job(1, job.until);
    let sink = Arc::clone(&poisoned.masks);
    assert!(
        std::thread::spawn(move || {
            let _guard = sink.lock().unwrap();
            panic!("fixture poisoning");
        })
        .join()
        .is_err()
    );
    assert_eq!(
        walk(
            &mut source,
            &poisoned,
            &LIMITS,
            &|| now,
            &mut FakePlatform(false)
        )
        .unwrap(),
        unavailable(CaptureId(1), Unavailable::Busy)
    );
    assert_eq!(
        poisoned
            .masks
            .lock()
            .unwrap_err()
            .into_inner()
            .completeness(),
        Completeness::Unknown(Unavailable::Busy)
    );
    assert_eq!(source.reads.get(), 0);
}

#[test]
fn navigation_failure_retains_the_read_node_including_a_password_sibling() {
    for (child, sibling, password, count, rects) in [
        (Some(0), None, false, 1, vec![]),
        (Some(1), None, false, 2, vec![]),
        (None, Some(1), true, 2, vec![RECT]),
    ] {
        let now = Instant::now();
        let job = job(1, now + Duration::from_secs(1));
        let mut source = FakeSource::flat(4);
        source.nodes[1].password = password;
        source.navigation_at = child;
        source.sibling_fail_at = sibling;
        let tree = walk(
            &mut source,
            &job,
            &LIMITS,
            &|| now,
            &mut FakePlatform(false),
        )
        .unwrap();
        assert_eq!(tree.elements.len(), count);
        assert_eq!(source.reads.get(), count);
        assert_eq!(tree.cut, Some(Cut::Provider));
        let masks = job.masks.lock().unwrap();
        assert_eq!(masks.rects(), rects);
        assert_eq!(masks.completeness(), Completeness::Partial);
    }
}

#[test]
fn empty_read_trees_require_the_cut_and_never_complete_masks() {
    for (fail, cut) in [(false, Cut::Budget), (true, Cut::Provider)] {
        let now = Instant::now();
        let job = job(
            1,
            if fail {
                now + Duration::from_secs(1)
            } else {
                now
            },
        );
        let mut source = FakeSource::flat(1);
        source.fail_at = fail.then_some(0);
        let tree = walk(
            &mut source,
            &job,
            &LIMITS,
            &|| now,
            &mut FakePlatform(false),
        )
        .unwrap();
        assert_eq!(tree.elements.len(), 0);
        assert_eq!(tree.availability, Availability::Read);
        assert_eq!(tree.cut, Some(cut));
        assert_eq!(
            job.masks.lock().unwrap().completeness(),
            Completeness::Partial
        );
    }
}
