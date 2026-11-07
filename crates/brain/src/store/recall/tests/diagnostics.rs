use super::*;

#[test]
fn sqlite_diagnostics_are_sanitized_before_actual_tracing_at_query_and_render_sites() {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    #[derive(Clone)]
    struct Log(Arc<Mutex<Vec<u8>>>);
    impl Write for Log {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let (_scratch, store) = world("recall-sql-diagnostics");
    task(&store, 2, "B");
    say(&store, 10, 2, "user", "needle");
    let marker = "sentinel_private_sql_diagnostic";
    store.conn.execute_batch("DROP TABLE events; CREATE VIEW events AS SELECT * FROM sentinel_private_sql_diagnostic;").unwrap();
    let raw=store.conn.prepare("SELECT e.thread,e.id FROM events_fts JOIN events e ON e.id=events_fts.rowid WHERE events_fts MATCH 'needle'").unwrap_err();
    assert!(
        raw.to_string().contains(marker),
        "the injected actual SQLite failure carries the marker"
    );
    let log = Log(Arc::new(Mutex::new(Vec::new())));
    let captured = Arc::clone(&log.0);
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || log.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let read = Snapshot::begin(&store.conn).unwrap();
        let error = read
            .select_recall(
                Origin::Kumo,
                &Query::Search {
                    query: "needle".into(),
                    task: None,
                },
            )
            .unwrap_err();
        assert_eq!(error, Error::Read);
        tracing::warn!(%error,?error,"a history query failed");
        let error = read
            .render_recall(
                Origin::Kumo,
                &Selection::Search {
                    query: "needle".into(),
                    hits: vec![Hit { task: 2, event: 10 }],
                    more: false,
                },
                &mut BTreeSet::new(),
            )
            .unwrap_err();
        assert!(matches!(error, StoreError::Read));
        tracing::warn!(%error,?error,"a context read failed");
    });
    let text = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    assert!(text.contains("can't read history"));
    assert!(text.contains("can't read the store page"));
    assert!(!text.contains(marker));
    assert!(!text.contains(&format!("{:?}", marker.as_bytes())));
}
