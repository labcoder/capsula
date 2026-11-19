use super::*;

#[test]
fn pending_cleanup_and_debug_do_not_reveal_the_chunk_count() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};

    struct Warnings(Arc<AtomicUsize>);
    impl<S: tracing::Subscriber> Layer<S> for Warnings {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            if *event.metadata().level() == tracing::Level::WARN {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    let setup = Setup::new("pending-diagnostic-redaction");
    let mut authority = setup.open();
    let warnings = Arc::new(AtomicUsize::new(0));
    let _logging = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(Warnings(Arc::clone(&warnings))),
    );
    for (bytes, chunks) in [(16, 1), (2048, 2)] {
        let plan = authority.begin(Kind::AnthropicKey, 0, bytes).expect("plan");
        assert_eq!(format!("{:?}", plan.layout()), "Layout(***)");
        assert_eq!(
            format!("{plan:?}"),
            "WritePlan { kind: AnthropicKey, generation: Generation(***), layout: Layout(***), base: 0 }"
        );
        {
            let mut state = setup.store.0.lock().expect("fake");
            state.refusing = true;
            state.mutations.clear();
        }
        io::delete_pending(
            &setup.store,
            SERVICE,
            Kind::AnthropicKey,
            plan.generation(),
            plan.layout(),
        );
        assert_eq!(setup.store.0.lock().expect("fake").mutations.len(), chunks);
        assert_eq!(warnings.swap(0, Ordering::Relaxed), 1);
    }
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
