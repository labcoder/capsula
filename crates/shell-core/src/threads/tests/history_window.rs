use super::*;
use kumochi_brain::store::Store;

const DAY: i64 = 86_400_000;

fn timestamp(id: u64) -> i64 {
    match id % 4 {
        1 | 2 => DAY,
        3 => DAY - 1,
        _ => DAY + 1,
    }
}

fn open() -> (crate::scratch::Scratch, Store, ThreadId, Threads) {
    let dir = crate::scratch::dir("history-window");
    let mut store = Store::open(&dir.join("store.db")).expect("store");
    store.recover().expect("main thread exists");
    let thread = store.main_thread().expect("main thread");
    let mut state = Threads::chat();
    state.connected();
    state.show_for(Some(thread));
    (dir, store, thread, state)
}

fn read(state: &mut Threads, store: &Store) -> (PageOf, DataToShell) {
    let mut history = None;
    for request in state.requests() {
        let bytes = frame::encode(&request, Pipe::Data).expect("page request frame");
        let decoded = frame::read::<DataToBrain>(&mut bytes.as_slice(), Pipe::Data)
            .expect("decode page request")
            .expect("page request frame");
        assert_eq!(decoded, request);
        let DataToBrain::PageRequest { request, page } = decoded else {
            panic!("only pages");
        };
        let reply = store
            .page_for_tests(request, page.clone())
            .expect("read page");
        if matches!(page, PageOf::Threads { .. }) {
            state.apply(reply);
        } else {
            assert!(history.replace((page, reply)).is_none());
        }
    }
    let (page, reply) = history.expect("one history page");
    let encoded = frame::encode(&reply, Pipe::Data).expect("bounded data frame");
    assert!(encoded.len() <= MAX_PAGE_BYTES);
    let decoded = frame::read::<DataToShell>(&mut encoded.as_slice(), Pipe::Data)
        .expect("decode page")
        .expect("page frame");
    assert_eq!(decoded, reply);
    (page, decoded)
}

fn chronological(state: &Threads) -> Vec<EventId> {
    state.events.iter().rev().map(|event| event.id).collect()
}

#[test]
fn bounded_history_walks_ten_thousand_real_events_in_both_directions() {
    const COUNT: u64 = 10_000;
    let (_dir, mut store, thread, mut state) = open();
    let wide = "é".repeat(700);
    let oversized = "界".repeat(MAX_PAGE_BYTES / 2);
    let seed: Vec<_> = (1..=COUNT)
        .map(|id| {
            let text = match id {
                1..=5_000 => "short",
                5_001 => oversized.as_str(),
                _ => wide.as_str(),
            };
            (timestamp(id), text)
        })
        .collect();
    store
        .append_history_for_tests(thread, &seed)
        .expect("seed history");

    let (mut edge, mut frontier) = (HistoryEdge::Older, COUNT + 1);
    let mut reached_row_cap = false;
    let mut reached_byte_cap = false;
    let mut saw_cut = [false; 2];
    loop {
        let previous = chronological(&state);
        let cursor = match edge {
            HistoryEdge::Older => state.before,
            HistoryEdge::Newer => state.after,
            HistoryEdge::Inside => unreachable!(),
        };
        state.history_scrolled(edge);
        let (page, reply) = read(&mut state, &store);
        assert_eq!(
            page,
            match edge {
                HistoryEdge::Older => PageOf::Events {
                    thread,
                    before: cursor
                },
                HistoryEdge::Newer => PageOf::EventsAfter {
                    thread,
                    after: cursor.expect("newer boundary"),
                },
                HistoryEdge::Inside => unreachable!(),
            },
        );
        let DataToShell::Page { events, next, .. } = &reply else {
            panic!("event page");
        };
        assert!((1..=MAX_PAGE_ROWS).contains(&events.len()));
        let older = edge == HistoryEdge::Older;
        let count = events.len();
        for index in 0..count {
            let event = &events[if older { index } else { count - 1 - index }];
            frontier = if older { frontier - 1 } else { frontier + 1 };
            assert_eq!(
                (event.id, event.created_ms),
                (EventId(frontier), timestamp(frontier))
            );
            assert_eq!(event.cut, frontier == 5_001);
            if event.cut {
                saw_cut[usize::from(!older)] = true;
                assert!(event.text.len() > MAX_PAGE_BYTES - 64);
                assert!(oversized.starts_with(&event.text));
            }
        }
        let more = if older {
            frontier > 1
        } else {
            frontier < COUNT
        };
        assert_eq!(*next, more.then_some(EventId(frontier)));
        state.apply(reply);
        let current = chronological(&state);
        assert!(current.windows(2).all(|pair| pair[0].0 + 1 == pair[1].0));
        let bytes: usize = state.events.iter().map(|event| event.text.len()).sum();
        assert!(current.len() <= 200 && bytes <= 128 * 1024);
        reached_row_cap |= current.len() == 200;
        reached_byte_cap |= current.len() < 200 && bytes > 128 * 1024 - wide.len();
        if let Some(anchor) = if older {
            previous.first()
        } else {
            previous.last()
        } {
            let top = if older { 0 } else { previous.len() };
            let restored = state
                .history_anchor(&previous, &current, top)
                .expect("visible anchor survives");
            assert_eq!(&current[restored], anchor);
        }
        if !more {
            if !older {
                break;
            }
            edge = HistoryEdge::Newer;
            frontier = current.last().expect("window").0;
        }
    }
    assert_eq!(frontier, COUNT);
    assert!(reached_row_cap && reached_byte_cap);
    assert_eq!(saw_cut, [true, true]);
    assert!(!state.has_newer_events());
}

#[test]
fn a_new_answer_leaves_the_scrolled_window_and_its_anchor_in_place() {
    let (_dir, mut store, thread, mut state) = open();
    store
        .append_history_for_tests(thread, &[(DAY, "old"); 60])
        .expect("history");
    let (_, newest) = read(&mut state, &store);
    state.apply(newest);
    let previous = chronological(&state);
    let anchor = previous[6];
    state.run_state(thread);
    let (_, reply_before_scroll) = read(&mut state, &store);
    state.history_scrolled(HistoryEdge::Inside);
    store
        .append_history_for_tests(thread, &[(DAY + 1, "new")])
        .expect("live append");
    state.run_state(thread);
    state.apply(reply_before_scroll);
    let (_, repair) = read(&mut state, &store);
    state.apply(repair);
    assert_eq!(chronological(&state), previous);
    assert!(state.has_newer_events());

    state.history_scrolled(HistoryEdge::Newer);
    let (page, new) = read(&mut state, &store);
    assert_eq!(
        page,
        PageOf::EventsAfter {
            thread,
            after: EventId(60)
        }
    );
    state.apply(new);
    let current = chronological(&state);
    assert_eq!(current.last(), Some(&EventId(61)));
    assert_eq!(state.history_anchor(&previous, &current, 6), Some(6));
    assert_eq!(current[6], anchor);
    assert_eq!(
        state.history_anchor(&previous, &current, previous.len()),
        Some(49)
    );
    assert!(!state.has_newer_events());

    state.history_scrolled(HistoryEdge::Newer);
    store
        .append_history_for_tests(thread, &[(DAY + 1, "newer")])
        .expect("tail append");
    state.run_state(thread);
    let (_, repair) = read(&mut state, &store);
    state.apply(repair);
    assert_eq!(state.events[0].id, EventId(62));
    assert_eq!(
        state.history_anchor(&current, &chronological(&state), current.len()),
        None
    );
}
