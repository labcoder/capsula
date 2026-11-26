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

#[test]
fn a_paste_plan_is_recorded_as_staged_and_never_activates() {
    let setup = Setup::new("paste-store");
    let mut authority = setup.open();
    for len in [15, 2049] {
        assert!(matches!(
            authority.begin_paste(0, len),
            Err(AuthorityError::Bounds)
        ));
    }
    let store_of = |generation: Generation| -> String {
        setup
            .raw()
            .query_row(
                "SELECT store FROM generations WHERE generation = ?1",
                [generation.0.as_slice()],
                |row| row.get(0),
            )
            .expect("store")
    };
    let private = authority.begin(Kind::AnthropicKey, 0, 16).expect("private");
    assert_eq!(private.store(), Store::Private);
    assert_eq!(store_of(private.generation()), "private");
    let paste = authority.begin_paste(0, 16).expect("paste");
    let generation = paste.generation();
    assert_eq!(paste.store(), Store::Staging);
    assert_eq!(store_of(generation), "staging");
    let verified = io::write(&setup.store, SERVICE, paste, &secret(3, 16)).expect("staged");
    assert!(matches!(
        authority.activate(verified, None),
        Err(AuthorityError::Conflict)
    ));
    assert_eq!(
        authority.current(Kind::AnthropicKey).expect("head"),
        Current {
            revision: 0,
            active: None
        }
    );
    for change in [
        "UPDATE generations SET state = 'active' WHERE generation = ?1",
        "UPDATE generations SET kind = 'host' WHERE generation = ?1",
    ] {
        assert!(
            setup
                .raw()
                .execute(change, [generation.0.as_slice()])
                .is_err(),
            "the file refuses an active or non-Anthropic staged row"
        );
    }
}

#[test]
fn retired_rows_keep_the_store_they_were_minted_in() {
    let setup = Setup::new("paste-retired-store");
    let mut authority = setup.open();
    let saved = setup.save(&mut authority, Kind::AnthropicKey, &secret(1, 16));
    let staged = authority.begin_paste(1, 16).expect("paste").generation();
    setup.save(&mut authority, Kind::AnthropicKey, &secret(2, 16));
    let retired: BTreeMap<String, Store> = authority
        .retired(16)
        .expect("retired")
        .into_iter()
        .map(|row| (row.generation.hex(), row.store()))
        .collect();
    assert_eq!(
        retired,
        BTreeMap::from([
            (saved.generation.hex(), Store::Private),
            (staged.hex(), Store::Staging),
        ])
    );
    assert_eq!(
        authority
            .retired_kind(Kind::AnthropicKey)
            .expect("retired")
            .iter()
            .map(Retired::store)
            .collect::<Vec<_>>(),
        [Store::Private, Store::Staging]
    );
    drop(authority);
    setup
        .raw()
        .execute(
            "UPDATE generations SET chunks = 2, bytes = 2049 WHERE generation = ?1",
            [staged.0.as_slice()],
        )
        .expect("a staged row no key fits");
    assert!(matches!(
        setup.open().retired(16),
        Err(AuthorityError::Corrupt)
    ));
}

#[test]
fn a_version_three_file_keeps_every_row_in_the_private_store() {
    let setup = Setup::new("auth-migrate-store");
    let raw = setup.raw();
    raw.execute_batch(SCHEMA).expect("version 3 heads");
    raw.execute_batch(GENERATIONS)
        .expect("version 3 generations");
    raw.execute(
        "INSERT INTO control (id, lifetime, marker) VALUES (1, 0, ?1)",
        [marker_json(SignOutMarker::default()).expect("marker")],
    )
    .expect("control");
    raw.execute_batch(
        "INSERT INTO generations (generation, kind, state, chunks, bytes) VALUES
             (X'01000000000000000000000000000000', 'host', 'active', 1, 45),
             (X'02000000000000000000000000000000', 'anthropic', 'pending', 1, 16),
             (X'03000000000000000000000000000000', 'github', 'removed', 1, 10);
         PRAGMA user_version = 3;",
    )
    .expect("version 3 rows");
    drop(raw);
    let mut authority = setup.open();
    let version: i64 = setup
        .raw()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");
    assert_eq!(version, 5);
    assert_eq!(
        authority
            .current(Kind::Host)
            .expect("host")
            .active
            .map(|active| active.generation.hex()),
        Some(format!("01{}", "0".repeat(30)))
    );
    let rows: Vec<(String, String, String)> = setup
        .raw()
        .prepare("SELECT hex(generation), state, store FROM generations ORDER BY rowid")
        .expect("rows")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("rows");
    let row = |first: &str, state: &str| {
        (
            format!("{first}{}", "0".repeat(30)),
            state.to_owned(),
            "private".to_owned(),
        )
    };
    assert_eq!(
        rows,
        [
            row("01", "active"),
            row("02", "removed"),
            row("03", "removed")
        ]
    );
    assert_eq!(
        authority.begin_paste(0, 16).expect("paste").store(),
        Store::Staging
    );
}

#[test]
fn an_active_row_outside_the_private_store_is_refused() {
    let setup = Setup::new("paste-active-staged");
    let mut authority = setup.open();
    let saved = setup.save(&mut authority, Kind::AnthropicKey, &secret(1, 16));
    drop(authority);
    setup
        .raw()
        .execute_batch(&format!(
            "PRAGMA ignore_check_constraints = ON;
             UPDATE generations SET store = 'staging' WHERE generation = X'{}';",
            saved.generation.hex()
        ))
        .expect("a malformed file");
    assert!(matches!(
        setup.open().current(Kind::AnthropicKey),
        Err(AuthorityError::Corrupt)
    ));
}

#[test]
fn the_skipped_repair_version_and_later_ones_are_refused_as_newer() {
    for version in [4, 6] {
        let setup = Setup::new(&format!("auth-newer-{version}"));
        drop(setup.open());
        setup
            .raw()
            .execute_batch(&format!("PRAGMA user_version = {version}"))
            .expect("newer");
        assert!(matches!(
            Authority::open(&setup.lock, Missing::Create),
            Err(AuthorityError::NewerSchema)
        ));
    }
}
