use super::*;

#[test]
fn pending_cleanup_and_debug_do_not_reveal_the_chunk_count() {
    use crate::test_support::Captured;

    let setup = Setup::new("pending-diagnostic-redaction");
    let mut authority = setup.open();
    setup.store.0.lock().expect("fake").refusing = true;
    let [small, large] = [16, 2048].map(|bytes| {
        let plan = authority.begin(Kind::AnthropicKey, 0, bytes).expect("plan");
        let captured = Captured::default();
        let _logging = captured.install();
        io::delete_pending(
            &setup.store,
            SERVICE,
            Kind::AnthropicKey,
            plan.generation(),
            plan.layout(),
        );
        let logs = captured.text();
        let warnings: Vec<_> = logs
            .lines()
            .filter_map(|line| line.split_once(" WARN ").map(|(_, payload)| payload))
            .collect();
        assert!(!warnings.is_empty(), "refused cleanup must be reported");
        let warning = warnings.join("\n");
        let debug = format!("{plan:?} {:?}", plan.layout());
        let generation = plan.generation().hex();
        let bytes = format!("{:?}", plan.generation().0);
        for shown in [&debug, &warning] {
            assert!(!shown.contains(&generation));
            assert!(!shown.contains(bytes.trim_matches(['[', ']'])));
        }
        (debug, warning)
    });
    assert!(small == large, "diagnostics must not disclose the layout");
}

#[test]
fn pending_read_waits_with_one_exact_deadline_across_both_chunks() {
    let setup = Setup::new("pending-read-deadline");
    let mut authority = setup.open();
    for (visible, success, elapsed) in [
        ([0, 0], true, 0),
        ([10, 90], true, 90),
        ([90, 100], false, 100),
        ([0, 101], false, 100),
    ] {
        let plan = authority.begin(Kind::AnthropicKey, 0, 2048).expect("plan");
        for (index, len) in [2000, 48].into_iter().enumerate() {
            setup
                .store
                .set(
                    &generation_target(SERVICE, "anthropic", &plan.generation().hex(), index),
                    &secret(7, len),
                )
                .expect("staged");
        }
        let clock = FakeClock::new();
        let late = Late {
            store: &setup.store,
            clock: &clock,
            visible: &visible,
        };
        let result =
            io::read_pending_within(&late, SERVICE, &plan, Duration::from_millis(100), &clock);
        if success {
            assert_eq!(
                result.expect("whole staged value").expose().as_slice(),
                &[7; 2048]
            );
        } else {
            assert!(matches!(result, Err(AuthorityError::Unsettled)));
        }
        assert_eq!(clock.elapsed(), Duration::from_millis(elapsed));
        assert_eq!(
            authority.current(Kind::AnthropicKey).expect("head").active,
            None
        );
    }
}

#[test]
fn pending_read_refuses_missing_malformed_and_failed_reads() {
    let setup = Setup::new("pending-read-invalid");
    let mut authority = setup.open();
    let plan = authority.begin(Kind::AnthropicKey, 0, 2048).expect("plan");
    let clock = FakeClock::new();
    assert!(matches!(
        io::read_pending_within(
            &setup.store,
            SERVICE,
            &plan,
            Duration::from_millis(20),
            &clock
        ),
        Err(AuthorityError::Unsettled)
    ));
    assert_eq!(clock.elapsed(), Duration::from_millis(20));
    let name = generation_target(SERVICE, "anthropic", &plan.generation().hex(), 0);
    setup
        .store
        .set(&name, &secret(7, 1999))
        .expect("short staged chunk");
    let clock = FakeClock::new();
    assert!(matches!(
        io::read_pending_within(
            &setup.store,
            SERVICE,
            &plan,
            Duration::from_millis(20),
            &clock
        ),
        Err(AuthorityError::Unsettled)
    ));
    assert_eq!(clock.elapsed(), Duration::ZERO);
    setup.store.0.lock().expect("fake").denied_read = Some(name);
    assert!(matches!(
        io::read_pending_within(
            &setup.store,
            SERVICE,
            &plan,
            Duration::from_millis(20),
            &clock
        ),
        Err(AuthorityError::Store(CredentialStoreError::Read(5)))
    ));
}
