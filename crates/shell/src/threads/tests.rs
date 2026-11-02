use super::*;
use capsula_protocol::message::{PageEventKind, RunId};

#[test]
fn search_debounce_invalidates_old_replies_without_sending_a_replacement_early() {
    let mut state = Threads::default();
    state.connected();
    let first = request_id(&state.requests()[0]);
    let old = state.defer_search("old");
    let current = state.defer_search("current");
    state.apply(reply(first, &[9], None));
    assert!(state.rows.is_empty());
    assert!(state.requests().is_empty());
    state.finish_search(old);
    assert!(
        state.requests().is_empty(),
        "the superseded timer can't unblock the query"
    );
    state.finish_search(current);
    assert_eq!(state.requests().len(), 1);
}

fn row(id: u64) -> ThreadRow {
    ThreadRow {
        id: ThreadId(id),
        title: format!("private title {id}"),
        updated_ms: i64::try_from(id).expect("small"),
    }
}

fn event(id: u64) -> PageEvent {
    PageEvent {
        id: EventId(id),
        run: Some(RunId(1)),
        kind: PageEventKind::User,
        text: "private event".to_owned(),
        cut: false,
    }
}

fn request_id(request: &DataToBrain) -> u32 {
    let DataToBrain::PageRequest { request, .. } = request else {
        panic!("page request")
    };
    *request
}

fn reply(request: u32, ids: &[u64], next: Option<u64>) -> DataToShell {
    DataToShell::ThreadList {
        request,
        threads: ids.iter().copied().map(row).collect(),
        next: next.map(|id| ThreadCursor {
            updated_ms: i64::try_from(id).expect("small"),
            id: ThreadId(id),
        }),
    }
}

fn loaded() -> Threads {
    let mut state = Threads::default();
    state.connected();
    let requests = state.requests();
    state.apply(reply(request_id(&requests[0]), &[3, 2], Some(2)));
    state
}

#[test]
fn superseded_search_replies_are_dropped_without_releasing_another_slot() {
    let mut state = Threads::default();
    state.connected();
    let first = request_id(&state.requests()[0]);
    state.search("new query");
    assert!(
        state.requests().is_empty(),
        "the old read still owns its slot"
    );
    state.apply(reply(first.wrapping_add(100), &[99], None));
    assert!(
        state.requests().is_empty(),
        "an unrelated reply frees nothing"
    );
    state.apply(reply(first, &[3], None));
    assert!(state.rows.is_empty());
    let request = state.requests();
    assert_eq!(request.len(), 1);
    assert_eq!(
        request[0],
        DataToBrain::PageRequest {
            request: first + 1,
            page: PageOf::Threads {
                search: Some("new query".to_owned()),
                after: None
            }
        }
    );
    state.apply(reply(first, &[3], None));
    assert!(state.rows.is_empty());
    state.apply(reply(first + 1, &[2], None));
    assert_eq!(state.rows, vec![row(2)]);
}

#[test]
fn returning_to_the_same_query_still_drops_its_superseded_reply() {
    let mut state = Threads::default();
    state.connected();
    state.search("a");
    let first = request_id(&state.requests()[0]);
    state.search("b");
    state.search("a");
    state.apply(reply(first, &[3], None));
    assert!(state.rows.is_empty());
    assert_eq!(state.requests().len(), 1);
}

#[test]
fn loading_more_sends_each_cursor_only_once_until_answered() {
    let mut state = loaded();
    state.more_threads();
    state.more_threads();
    let requests = state.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0],
        DataToBrain::PageRequest {
            request: 2,
            page: PageOf::Threads {
                search: None,
                after: state.next
            }
        }
    );
    state.more_threads();
    assert!(state.requests().is_empty());
    state.apply(reply(2, &[1], None));
    assert_eq!(state.rows, vec![row(3), row(2), row(1)]);
    state.more_threads();
    assert!(state.requests().is_empty());
}

#[test]
fn reconnect_rereads_both_first_pages_with_new_request_ids() {
    let mut state = loaded();
    state.select(Some(ThreadId(2)));
    let old = request_id(&state.requests()[0]);
    state.disconnected();
    assert!(!state.listing() && !state.reading());
    assert!(state.requests().is_empty());
    state.connected();
    let requests = state.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0],
        DataToBrain::PageRequest {
            request: old + 1,
            page: PageOf::Threads {
                search: None,
                after: None
            }
        }
    );
    assert_eq!(
        requests[1],
        DataToBrain::PageRequest {
            request: old + 2,
            page: PageOf::Events {
                thread: ThreadId(2),
                before: None
            }
        }
    );
    state.apply(DataToShell::Page {
        request: old,
        thread: ThreadId(2),
        events: vec![event(1)],
        before: None,
    });
    assert!(state.events.is_empty());
}

#[test]
fn empty_list_and_empty_search_finish_loading_and_have_no_more_pages() {
    let mut state = Threads::default();
    state.connected();
    let first = request_id(&state.requests()[0]);
    state.apply(reply(first, &[], None));
    assert!(state.listed && state.rows.is_empty() && !state.searching());
    assert!(!state.listing());
    state.search("nothing");
    let next = request_id(&state.requests()[0]);
    state.apply(reply(next, &[], None));
    assert!(state.listed && state.rows.is_empty() && state.searching());
    state.more_threads();
    assert!(state.requests().is_empty());
}

#[test]
fn a_run_on_a_nonfirst_thread_repairs_once_and_keeps_the_older_cursor() {
    let mut state = loaded();
    state.more_threads();
    let more = request_id(&state.requests()[0]);
    state.apply(reply(more, &[1], Some(1)));
    let cursor = state.next;
    state.run_state(ThreadId(2));
    state.run_state(ThreadId(2));
    let requests = state.requests();
    assert_eq!(requests.len(), 1);
    assert!(matches!(
        requests[0],
        DataToBrain::PageRequest {
            page: PageOf::Threads {
                search: None,
                after: None
            },
            ..
        }
    ));
    state.run_state(ThreadId(2));
    assert!(state.requests().is_empty());
    state.apply(reply(request_id(&requests[0]), &[2, 3], None));
    assert_eq!(state.rows, vec![row(2), row(3), row(1)]);
    assert_eq!(state.next, cursor);
    state.run_state(ThreadId(2));
    assert!(state.requests().is_empty(), "already first");
}

#[test]
fn an_unlisted_run_repairs_after_a_pending_older_page_but_search_is_not_repaired() {
    let mut state = loaded();
    state.more_threads();
    let more = request_id(&state.requests()[0]);
    state.run_state(ThreadId(9));
    assert!(state.requests().is_empty());
    state.apply(reply(more, &[1], None));
    let repair = request_id(&state.requests()[0]);
    state.apply(reply(repair, &[9, 3], Some(3)));
    assert_eq!(state.rows, vec![row(9), row(3), row(2), row(1)]);
    state.search("match");
    let search = request_id(&state.requests()[0]);
    state.apply(reply(search, &[2], None));
    state.run_state(ThreadId(3));
    assert!(state.requests().is_empty());
}

#[test]
fn switching_or_closing_threads_drops_old_events_and_never_sends_two_event_reads() {
    let mut state = loaded();
    state.select(Some(ThreadId(3)));
    let first = request_id(&state.requests()[0]);
    state.select(Some(ThreadId(2)));
    assert!(state.requests().is_empty());
    state.apply(DataToShell::Page {
        request: first,
        thread: ThreadId(3),
        events: vec![event(5)],
        before: None,
    });
    assert!(state.events.is_empty());
    let next = request_id(&state.requests()[0]);
    state.select(None);
    state.apply(DataToShell::Page {
        request: next,
        thread: ThreadId(2),
        events: vec![event(4)],
        before: None,
    });
    assert!(state.events.is_empty());
    assert!(state.requests().is_empty());
    assert_eq!(state.open, None);
}

#[test]
fn older_events_continue_once_and_an_empty_first_page_marks_the_thread_gone() {
    let mut state = loaded();
    state.select(Some(ThreadId(3)));
    let first = request_id(&state.requests()[0]);
    state.apply(DataToShell::Page {
        request: first,
        thread: ThreadId(3),
        events: vec![event(5), event(4)],
        before: Some(EventId(4)),
    });
    state.older_events();
    let requests = state.requests();
    assert_eq!(
        requests[0],
        DataToBrain::PageRequest {
            request: first + 1,
            page: PageOf::Events {
                thread: ThreadId(3),
                before: Some(EventId(4))
            }
        }
    );
    state.older_events();
    assert!(state.requests().is_empty());
    state.apply(DataToShell::Page {
        request: first + 1,
        thread: ThreadId(3),
        events: vec![event(3)],
        before: None,
    });
    assert_eq!(state.events, vec![event(5), event(4), event(3)]);
    state.select(Some(ThreadId(99)));
    let missing = request_id(&state.requests()[0]);
    state.apply(DataToShell::Page {
        request: missing,
        thread: ThreadId(99),
        events: vec![],
        before: None,
    });
    assert!(state.loaded && state.events.is_empty());
}

#[test]
fn a_run_in_the_open_thread_rereads_its_newest_events_and_keeps_older_ones() {
    let events_request = |requests: Vec<DataToBrain>| {
        requests
            .into_iter()
            .find(|request| {
                matches!(
                    request,
                    DataToBrain::PageRequest {
                        page: PageOf::Events { .. },
                        ..
                    }
                )
            })
            .expect("an event read")
    };
    let mut state = loaded();
    state.select(Some(ThreadId(3)));
    let first = request_id(&state.requests()[0]);
    state.apply(DataToShell::Page {
        request: first,
        thread: ThreadId(3),
        events: vec![event(5), event(4)],
        before: Some(EventId(4)),
    });
    state.run_state(ThreadId(8));
    assert!(state.requests().iter().all(|request| !matches!(
        request,
        DataToBrain::PageRequest {
            page: PageOf::Events { .. },
            ..
        }
    )));
    state.run_state(ThreadId(3));
    let repair = events_request(state.requests());
    let DataToBrain::PageRequest { request, page } = repair else {
        unreachable!("filtered above")
    };
    assert_eq!(
        page,
        PageOf::Events {
            thread: ThreadId(3),
            before: None
        }
    );
    state.apply(DataToShell::Page {
        request,
        thread: ThreadId(3),
        events: vec![event(7), event(6), event(5)],
        before: Some(EventId(5)),
    });
    assert_eq!(
        state.events,
        vec![event(7), event(6), event(5), event(4)],
        "new events lead, nothing repeats"
    );
    assert_eq!(state.before, Some(EventId(4)), "older paging continues");
}

#[test]
fn an_oversized_search_never_queues_an_unencodable_request_and_can_be_corrected() {
    let mut state = loaded();
    state.search(&"x".repeat(Pipe::Data.max_payload()));
    assert!(state.search_error.is_some());
    assert!(state.requests().is_empty());
    state.search("short");
    assert!(state.search_error.is_none());
    assert_eq!(state.requests().len(), 1);
}

#[test]
fn thread_state_debug_never_includes_user_content() {
    let mut state = loaded();
    state.search("private search");
    state.events.push(event(1));
    state.rows.push(row(9));
    let debug = format!("{state:?}");
    for content in ["private title", "private search", "private event"] {
        assert!(!debug.contains(content));
    }
}
