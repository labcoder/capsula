use super::*;
use kumochi_protocol::message::{PageEventKind, RunId, ThreadKind};

fn settings(name: &str) -> SettingsView {
    SettingsView {
        assistant_name: kumochi_protocol::message::AssistantName::new(name).expect("valid"),
        user_name: None,
        onboarded: true,
        main_thread: ThreadId(1),
        max_frame_rate: kumochi_protocol::message::MaxFrameRate::AUTOMATIC,
        default_model: None,
    }
}

#[test]
fn main_before_settings_uses_the_default_assistant_name() {
    let mut main = row(1);
    main.kind = ThreadKind::Main;
    main.title.clear();
    assert_eq!(row_label(&main, None), constants::DEFAULT_ASSISTANT_NAME);
}

#[test]
fn a_rename_keeps_the_confirmed_title_until_its_original_page_is_read() {
    let mut state = loaded();
    state.more_threads();
    let first = state.requests().remove(0);
    let DataToBrain::PageRequest {
        page: PageOf::Threads { after: cursor, .. },
        ..
    } = first
    else {
        panic!("list")
    };
    state.apply(reply(request_id(&first), &[1], None));
    state.renamed(ThreadId(1));
    assert_eq!(
        state.row(ThreadId(1)).expect("cached").title,
        "private title 1"
    );
    assert_eq!(state.rows[2].title, "private title 1");
    let reread = state.requests().remove(0);
    assert_eq!(
        reread,
        DataToBrain::PageRequest {
            request: 3,
            page: PageOf::Threads {
                search: None,
                after: cursor
            }
        }
    );
    let mut renamed = row(1);
    renamed.title = "confirmed new title".to_owned();
    state.apply(DataToShell::ThreadList {
        request: request_id(&reread),
        threads: vec![renamed.clone()],
        next: None,
    });
    assert_eq!(state.row(ThreadId(1)), Some(&renamed));
    assert_eq!(state.rows[2], renamed);
    state.renamed(ThreadId(1));
    let failed_write = state.requests().remove(0);
    state.apply(DataToShell::ThreadList {
        request: request_id(&failed_write),
        threads: vec![row(0)],
        next: None,
    });
    assert_eq!(
        state.rows[2], renamed,
        "a refused write never invents a title"
    );
    assert_eq!(state.row(ThreadId(1)), Some(&renamed));
}

#[test]
fn a_rename_during_a_page_read_waits_then_rereads_after_the_op() {
    let mut state = loaded();
    state.refresh();
    let old = state.requests().remove(0);
    state.renamed(ThreadId(3));
    assert_eq!(state.requests(), []);
    state.apply(reply(request_id(&old), &[3, 2], Some(2)));
    assert_eq!(
        state.row(ThreadId(3)).expect("row").title,
        "private title 3"
    );
    assert_eq!(
        state.requests().len(),
        1,
        "the pre-op reply didn't confirm the rename"
    );
}

#[test]
fn a_delete_during_a_rename_rereads_the_offpage_title_without_resurrecting_the_deleted_row() {
    let mut state = with_second_page();
    state.renamed(ThreadId(1));
    let old = state.requests().remove(0);
    state.deleted(ThreadId(0));
    let mut renamed = row(1);
    renamed.title = "confirmed new title".to_owned();
    state.apply(DataToShell::ThreadList {
        request: request_id(&old),
        threads: vec![renamed.clone(), row(0)],
        next: None,
    });
    assert_eq!(state.row(ThreadId(0)), None);
    assert_eq!(state.row(ThreadId(1)), Some(&row(1)));
    assert_eq!(state.rows, [row(3), row(2), row(1)]);

    assert_eq!(
        state.requests(),
        [DataToBrain::PageRequest {
            request: 4,
            page: PageOf::Threads {
                search: None,
                after: Some(ThreadCursor {
                    updated_ms: 2,
                    id: ThreadId(2),
                    rank_bits: 0,
                }),
            },
        }]
    );
    state.apply(DataToShell::ThreadList {
        request: 4,
        threads: vec![renamed.clone()],
        next: None,
    });
    assert_eq!(state.row(ThreadId(1)), Some(&renamed));
    assert_eq!(state.rows, [row(3), row(2), renamed.clone()]);
    assert_eq!(state.requests(), [list_request(5)]);
    state.apply(reply(5, &[3, 2], None));
    assert_eq!(state.rows, [row(3), row(2), renamed]);
    assert_eq!(state.row(ThreadId(0)), None);
    assert_eq!(state.requests(), []);
}

#[test]
fn a_search_before_delete_does_not_requeue_an_old_rename_cursor() {
    let mut state = with_second_page();
    state.renamed(ThreadId(1));
    let old = state.requests().remove(0);
    state.search("new query");
    state.deleted(ThreadId(0));
    state.apply(reply(request_id(&old), &[1, 0], None));
    assert_eq!(state.rows, []);
    assert_eq!(state.row(ThreadId(0)), None);
    assert_eq!(
        state.requests(),
        [DataToBrain::PageRequest {
            request: 4,
            page: PageOf::Threads {
                search: Some("new query".to_owned()),
                after: None,
            },
        }]
    );
    state.apply(reply(4, &[8], None));
    assert_eq!(state.rows, [row(8)]);
    assert_eq!(state.requests(), []);
}

#[test]
fn a_refresh_after_delete_discards_the_old_rename_cursor() {
    let mut state = with_second_page();
    state.renamed(ThreadId(1));
    let old = state.requests().remove(0);
    state.deleted(ThreadId(0));
    state.refresh();
    state.apply(reply(request_id(&old), &[1, 0], None));
    assert_eq!(state.row(ThreadId(0)), None);
    assert_eq!(state.requests(), [list_request(4)]);
    state.apply(reply(4, &[3, 2], None));
    assert_eq!(state.rows, [row(3), row(2)]);
    assert_eq!(state.requests(), []);
}

#[test]
fn hiding_after_delete_discards_the_old_rename_cursor() {
    let mut state = with_second_page();
    state.renamed(ThreadId(1));
    let old = state.requests().remove(0);
    state.deleted(ThreadId(0));
    state.hide();
    state.apply(reply(request_id(&old), &[1, 0], None));
    assert_eq!(state.row(ThreadId(0)), None);
    assert!(state.reread.is_empty());
    assert_eq!(state.requests(), []);
    state.show();
    assert_eq!(state.requests(), [list_request(4)]);
    state.apply(reply(4, &[3, 2], None));
    assert_eq!(state.rows, [row(3), row(2)]);
    assert_eq!(state.requests(), []);
}

#[test]
fn a_delete_while_hidden_does_not_requeue_an_old_rename_cursor() {
    let mut state = with_second_page();
    state.renamed(ThreadId(1));
    let old = state.requests().remove(0);
    state.hide();
    state.deleted(ThreadId(0));
    state.apply(reply(request_id(&old), &[1, 0], None));
    assert_eq!(state.row(ThreadId(0)), None);
    assert!(state.reread.is_empty());
    assert_eq!(state.requests(), []);
    state.show();
    assert_eq!(state.requests(), [list_request(4)]);
    state.apply(reply(4, &[3, 2], None));
    assert_eq!(state.rows, [row(3), row(2)]);
    assert_eq!(state.requests(), []);
}

#[test]
fn kumo_in_search_and_the_cache_uses_only_the_latest_settings_name() {
    let mut state = loaded();
    state.set_settings(settings("Kumo"));
    state.search("a remembered phrase");
    let request = state.requests().remove(0);
    let mut main = row(1);
    main.kind = ThreadKind::Main;
    main.title.clear();
    state.apply(DataToShell::ThreadList {
        request: request_id(&request),
        threads: vec![row(9), main],
        next: None,
    });
    assert_eq!(row_label(&state.rows[1], state.settings.as_ref()), "Kumo");
    assert_eq!(
        row_label(
            state.row(ThreadId(1)).expect("cached"),
            state.settings.as_ref()
        ),
        "Kumo"
    );
    assert_eq!(
        row_label(&state.rows[0], state.settings.as_ref()),
        "private title 9"
    );
    state.set_settings(settings("Cloud"));
    assert_eq!(row_label(&state.rows[1], state.settings.as_ref()), "Cloud");
    assert_eq!(
        row_label(
            state.row(ThreadId(1)).expect("cached"),
            state.settings.as_ref()
        ),
        "Cloud"
    );
    assert_eq!(state.requests(), [], "a name update needs no list read");
}

#[test]
fn switch_uses_the_received_title_and_choice() {
    use crate::runs::{Runs, Target};
    use kumochi_protocol::message::{Effort, ModelChoice, ModelSlug};
    let mut state = loaded();
    let mut chosen = row(9);
    chosen.choice = Some(ModelChoice {
        model: ModelSlug::new("stored-model").expect("valid"),
        effort: Effort::High,
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
    });
    state.refresh();
    let request = state.requests().remove(0);
    state.apply(DataToShell::ThreadList {
        request: request_id(&request),
        threads: vec![chosen.clone()],
        next: None,
    });
    let mut runs = Runs::default();
    runs.settings(ThreadId(1));
    state.select(Some(ThreadId(9)));
    assert_eq!(state.open, Some(ThreadId(9)));
    assert!(runs.select_row(state.row(ThreadId(9)).expect("received row")));
    state.search("another query");
    assert_eq!(state.row(ThreadId(9)), Some(&chosen));
    assert_eq!(
        row_label(
            state.row(ThreadId(9)).expect("cache"),
            state.settings.as_ref()
        ),
        "private title 9"
    );
    assert_eq!(
        state.row(ThreadId(9)).expect("cache").choice,
        Some(ModelChoice {
            model: ModelSlug::new("stored-model").expect("valid"),
            effort: Effort::High,
            connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
        })
    );
    assert_eq!(runs.target(), Target::Task(ThreadId(9)));
    assert_eq!(runs.revision(), 1);
}

#[test]
fn search_debounce_invalidates_old_replies_without_sending_a_replacement_early() {
    let mut state = Threads::default();
    state.show();
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
        kind: ThreadKind::Thread,
        choice: None,
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
            rank_bits: 0,
        }),
    }
}

fn loaded() -> Threads {
    let mut state = Threads::default();
    state.show();
    state.connected();
    let requests = state.requests();
    state.apply(reply(request_id(&requests[0]), &[3, 2], Some(2)));
    state
}

fn with_second_page() -> Threads {
    let mut state = loaded();
    state.more_threads();
    let request = state.requests().remove(0);
    state.apply(reply(request_id(&request), &[1, 0], None));
    state
}

#[test]
fn programmatic_view_changes_and_reopening_keep_the_highlight_in_sync() {
    let mut state = loaded();
    state.select(Some(ThreadId(2)));
    assert_eq!(state.selected_index(), Some(1));
    state.select(Some(ThreadId(3)));
    assert_eq!(state.selected_index(), Some(0));
    state.hide();
    assert_eq!(state.open, None);
    assert_eq!(state.selected_index(), None);
    state.show();
    assert_eq!(state.selected_index(), None);
}

#[test]
fn rename_requires_a_connected_row_that_is_not_deleting() {
    let mut state = loaded();
    assert!(state.can_rename(ThreadId(3)));
    state.deleting.push(ThreadId(3));
    assert!(!state.can_rename(ThreadId(3)));
    assert!(state.can_rename(ThreadId(2)));
    state.disconnected();
    assert!(!state.can_rename(ThreadId(2)));
}

#[test]
fn superseded_search_replies_are_dropped_without_releasing_another_slot() {
    let mut state = Threads::default();
    state.show();
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
    state.show();
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
    state.show();
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
    assert!(state.requests().is_empty(), "one read in flight");
    state.apply(reply(request_id(&requests[0]), &[2, 3], None));
    assert_eq!(state.rows, vec![row(2), row(3), row(1)]);
    assert_eq!(state.next, cursor);
    let followup = state.requests();
    assert_eq!(
        followup.len(),
        1,
        "the RunState during the read earns one more"
    );
    state.apply(reply(request_id(&followup[0]), &[2, 3], None));
    assert_eq!(state.rows, vec![row(2), row(3), row(1)]);
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
fn runs_on_either_side_of_a_repair_s_snapshot_both_reach_the_list() {
    let mut state = loaded();
    state.run_state(ThreadId(4));
    let repair = request_id(&state.requests()[0]);
    // Thread 5 is created after the store read thread 4's repair, but before its reply lands.
    state.run_state(ThreadId(5));
    assert!(state.requests().is_empty(), "one read in flight");
    state.apply(reply(repair, &[4, 3], Some(3)));
    assert_eq!(state.rows, vec![row(4), row(3), row(2)]);
    let followup = request_id(&state.requests()[0]);
    state.apply(reply(followup, &[5, 4, 3], Some(3)));
    assert_eq!(state.rows, vec![row(5), row(4), row(3), row(2)]);
    assert!(state.requests().is_empty());
}

#[test]
fn a_repair_that_misses_the_shown_events_keeps_the_gap_reachable() {
    let mut state = loaded();
    state.select(Some(ThreadId(3)));
    let first = request_id(&state.requests()[0]);
    state.apply(DataToShell::Page {
        request: first,
        thread: ThreadId(3),
        events: vec![event(5), event(4)],
        before: Some(EventId(4)),
    });
    state.run_state(ThreadId(3));
    let repair = state
        .requests()
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
        .expect("an event read");
    // The byte budget cut the page at 8, above everything shown.
    state.apply(DataToShell::Page {
        request: request_id(&repair),
        thread: ThreadId(3),
        events: vec![event(9), event(8)],
        before: Some(EventId(8)),
    });
    state.older_events();
    let older = state.requests();
    assert_eq!(
        older[0],
        DataToBrain::PageRequest {
            request: request_id(&older[0]),
            page: PageOf::Events {
                thread: ThreadId(3),
                before: Some(EventId(8))
            }
        }
    );
    state.apply(DataToShell::Page {
        request: request_id(&older[0]),
        thread: ThreadId(3),
        events: vec![event(7), event(6), event(5), event(4)],
        before: None,
    });
    let ids: Vec<u64> = state.events.iter().map(|event| event.id.0).collect();
    assert_eq!(ids, [9, 8, 7, 6, 5, 4]);
    assert_eq!(state.before, None);
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

fn list_request(request: u32) -> DataToBrain {
    DataToBrain::PageRequest {
        request,
        page: PageOf::Threads {
            search: None,
            after: None,
        },
    }
}

#[test]
fn nothing_is_read_while_the_panel_is_closed() {
    let mut state = Threads::default();
    state.connected();
    state.run_state(ThreadId(4));
    assert_eq!(state.requests(), Vec::new());
    state.show();
    assert_eq!(state.requests(), vec![list_request(1)]);
    state.apply(reply(1, &[4], None));
    state.select(Some(ThreadId(4)));
    state.hide();
    state.run_state(ThreadId(5));
    assert_eq!(state.requests(), Vec::new());
    assert_eq!(state.open, None);
}

#[test]
fn a_failed_read_frees_its_slot_and_shows_until_a_page_lands() {
    let mut state = loaded();
    state.select(Some(ThreadId(3)));
    let events = request_id(&state.requests()[0]);
    state.apply(DataToShell::PageFailed { request: events });
    assert!(state.failed);
    assert!(!state.reading());
    assert_eq!(
        state.requests(),
        Vec::new(),
        "a failure doesn't retry by itself"
    );
    state.refresh();
    let list = request_id(&state.requests()[0]);
    state.apply(DataToShell::PageFailed {
        request: list.wrapping_add(1),
    });
    assert!(state.listing(), "another request's failure frees nothing");
    state.apply(DataToShell::PageFailed { request: list });
    assert!(!state.listing());
    assert_eq!(
        state.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [ThreadId(3), ThreadId(2)]
    );
    state.refresh();
    let list = request_id(&state.requests()[0]);
    state.apply(reply(list, &[7], None));
    assert!(!state.failed);
    assert_eq!(
        state.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [ThreadId(7)]
    );
}

#[test]
fn a_dropped_page_still_earns_its_whole_credit() {
    let mut state = loaded();
    let unsolicited = DataToShell::Page {
        request: 99,
        thread: ThreadId(3),
        events: vec![event(4)],
        before: None,
    };
    assert_eq!(state.receive(unsolicited, 321), 321);
    assert!(state.events.is_empty());
    assert_eq!(
        state.receive(DataToShell::PageFailed { request: 98 }, 17),
        17
    );
    assert!(!state.failed);
}

#[test]
fn a_superseded_event_read_that_fails_frees_its_slot_without_the_error() {
    let mut state = loaded();
    state.select(Some(ThreadId(3)));
    let old = request_id(&state.requests()[0]);
    state.select(Some(ThreadId(2)));
    state.apply(DataToShell::PageFailed { request: old });
    assert!(!state.failed);
    assert_eq!(
        state.requests(),
        vec![DataToBrain::PageRequest {
            request: old + 1,
            page: PageOf::Events {
                thread: ThreadId(2),
                before: None,
            },
        }]
    );
}

#[test]
fn a_replaced_search_that_fails_frees_its_slot_without_the_error() {
    let mut state = Threads::default();
    state.show();
    state.connected();
    assert_eq!(state.requests(), vec![list_request(1)]);
    state.search("new query");
    state.apply(DataToShell::PageFailed { request: 1 });
    assert!(!state.failed);
    assert_eq!(
        state.requests(),
        vec![DataToBrain::PageRequest {
            request: 2,
            page: PageOf::Threads {
                search: Some("new query".to_owned()),
                after: None,
            },
        }]
    );
}
