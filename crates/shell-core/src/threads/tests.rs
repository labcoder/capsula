use super::*;
use kumochi_protocol::message::{PageEventKind, RunId, ThreadKind};

fn issue_page(request: u32, thread: u64) -> DataToShell {
    DataToShell::Page {
        request,
        thread: ThreadId(thread),
        events: vec![PageEvent {
            id: EventId(9),
            run: None,
            kind: PageEventKind::Issue { draft: DraftId(9) },
            text: "title".into(),
            cut: false,
        }],
        before: None,
    }
}

fn full_issue(
    request: u32,
    version: u64,
    state: kumochi_protocol::message::ActionState,
) -> DataToShell {
    use kumochi_protocol::message::{ContentHash, IssueBody, IssueCard, IssueTitle, StateVersion};
    DataToShell::IssueCardRead {
        request,
        card: IssueCard {
            draft: DraftId(9),
            thread: ThreadId(3),
            repo: None,
            account: None,
            title: IssueTitle::new("private-card-marker").unwrap(),
            body: IssueBody::new("private-card-marker").unwrap(),
            hash: ContentHash([7; 32]),
            approval: None,
            version: StateVersion(version),
            state,
        },
    }
}

#[test]
fn issue_unavailable_releases_only_pending_identity_and_preserves_shared_authority() {
    use kumochi_protocol::message::{
        ActionState, IssueCardsStatus, IssueRefused, IssueUnavailableKind, StateVersion,
    };
    let (mut browse, mut chat) = Threads::pair();
    browse.connected();
    browse.show_for(Some(ThreadId(3)));
    chat.connected();
    chat.show_for(Some(ThreadId(3)));
    assert_eq!(
        browse.requests().iter().map(request_id).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        chat.requests().iter().map(request_id).collect::<Vec<_>>(),
        [3, 4]
    );
    browse.apply(issue_page(2, 3));
    chat.apply(issue_page(4, 3));
    assert_eq!(
        browse.requests(),
        [DataToBrain::IssueCardsRequest {
            request: 5,
            drafts: Bounded::new(vec![DraftId(9)]).unwrap(),
        }]
    );
    browse.apply(full_issue(5, 1, ActionState::Draft));
    browse.apply(DataToShell::IssueCardsDone {
        request: 5,
        status: IssueCardsStatus::Complete,
    });
    let expected = browse.issue(DraftId(9)).unwrap().card().unwrap().clone();
    browse.issue_submitted(DraftId(9), Some(71));
    browse.issue_unavailable(72, DraftId(9), IssueUnavailableKind::Action);
    browse.issue_unavailable(71, DraftId(10), IssueUnavailableKind::Action);
    for model in [&browse, &chat] {
        let entry = model.issue(DraftId(9)).unwrap();
        assert!(entry.pending());
        assert_eq!(entry.feedback, None);
    }
    chat.issue_unavailable(71, DraftId(9), IssueUnavailableKind::Action);
    for model in [&browse, &chat] {
        let entry = model.issue(DraftId(9)).unwrap();
        assert!(!entry.pending());
        assert!(!entry.loading());
        assert_eq!(
            entry.feedback,
            Some(IssueFeedback::Unavailable(IssueUnavailableKind::Action))
        );
        assert_eq!(entry.card(), Some(&expected));
        let reads = model.reads.borrow();
        let mark = reads.cards.entries[&DraftId(9)].mark.as_ref().unwrap();
        assert_eq!(
            (mark.version, &mark.state, mark.approval),
            (StateVersion(1), &ActionState::Draft, None)
        );
    }
    assert_eq!(browse.requests(), []);
    assert_eq!(chat.requests(), []);
    browse.issue_submitted(DraftId(9), Some(72));
    browse.issue_unavailable(71, DraftId(9), IssueUnavailableKind::Action);
    assert!(chat.issue(DraftId(9)).unwrap().pending());
    assert_eq!(chat.issue(DraftId(9)).unwrap().feedback, None);
    browse.action_state(
        DraftId(9),
        StateVersion(1),
        ActionState::Draft,
        None,
        Some(72),
    );
    browse.issue_unavailable(72, DraftId(9), IssueUnavailableKind::Action);
    assert!(!chat.issue(DraftId(9)).unwrap().pending());
    assert_eq!(chat.issue(DraftId(9)).unwrap().feedback, None);
    browse.issue_refused(
        72,
        DraftId(9),
        StateVersion(1),
        None,
        IssueRefused::NotAllowedNow,
    );
    browse.issue_unavailable(72, DraftId(9), IssueUnavailableKind::Action);
    assert_eq!(
        chat.issue(DraftId(9)).unwrap().feedback,
        Some(IssueFeedback::Refused(IssueRefused::NotAllowedNow))
    );
    browse.issue_submitted(DraftId(9), None);
    browse.issue_unavailable(72, DraftId(9), IssueUnavailableKind::Action);
    assert_eq!(
        chat.issue(DraftId(9)).unwrap().feedback,
        Some(IssueFeedback::QueueFailed)
    );
    assert_eq!(chat.issue(DraftId(9)).unwrap().card(), Some(&expected));
    assert_eq!(browse.requests(), []);
    assert_eq!(chat.requests(), []);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one shared-cache scenario covers watermarks, hydration and terminal pacing"
)]
fn displayed_issue_pages_use_one_shared_watermarked_card_and_terminal_pacing() {
    use kumochi_protocol::message::{ActionState, IssueCardsStatus, StateVersion};
    let (mut browse, mut chat) = Threads::pair();
    browse.connected();
    browse.show_for(Some(ThreadId(3)));
    chat.connected();
    chat.show_for(Some(ThreadId(3)));
    assert_eq!(
        browse.requests().iter().map(request_id).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        chat.requests().iter().map(request_id).collect::<Vec<_>>(),
        [3, 4]
    );
    browse.apply(issue_page(2, 3));
    assert_eq!(
        browse.requests(),
        [DataToBrain::IssueCardsRequest {
            request: 5,
            drafts: Bounded::new(vec![DraftId(9)]).unwrap()
        }]
    );
    chat.apply(issue_page(4, 3));
    assert_eq!(chat.requests(), []);
    browse.action_state(
        DraftId(9),
        StateVersion(2),
        ActionState::Sending,
        None,
        None,
    );
    browse.apply(full_issue(5, 1, ActionState::Draft));
    assert!(
        browse.reads.borrow().cards.entries[&DraftId(9)]
            .full
            .is_none()
    );
    browse.apply(DataToShell::IssueCardsDone {
        request: 5,
        status: IssueCardsStatus::Failed,
    });
    assert_eq!(
        browse.requests(),
        [DataToBrain::IssueCardsRequest {
            request: 6,
            drafts: Bounded::new(vec![DraftId(9)]).unwrap()
        }]
    );
    browse.apply(full_issue(6, 2, ActionState::Sending));
    assert_eq!(
        chat.reads.borrow().cards.entries[&DraftId(9)]
            .full
            .as_ref()
            .unwrap()
            .version,
        StateVersion(2)
    );
    assert_eq!(
        chat.reads.borrow().cards.entries[&DraftId(9)]
            .full
            .as_ref()
            .unwrap()
            .state,
        ActionState::Sending
    );
    assert!(browse.reads.borrow().pending.contains(&(6, View::Browse)));
    let debug = format!("{:?}", browse.reads.borrow().cards);
    assert!(!debug.contains("private-card-marker"));
    assert!(!debug.contains(&format!("{:?}", b"private-card-marker")));
    browse.action_state(DraftId(9), StateVersion(1), ActionState::Draft, None, None);
    assert_eq!(
        browse.reads.borrow().cards.entries[&DraftId(9)]
            .mark
            .as_ref()
            .unwrap()
            .version,
        StateVersion(2)
    );
    browse.action_state(DraftId(9), StateVersion(2), ActionState::Draft, None, None);
    assert!(browse.cards_broken());
    browse.apply(full_issue(6, 3, ActionState::Draft));
    assert_eq!(
        browse.reads.borrow().cards.entries[&DraftId(9)]
            .full
            .as_ref()
            .unwrap()
            .version,
        StateVersion(2)
    );
    assert_eq!(browse.requests(), []);
    browse.hide();
    assert_eq!(chat.reads.borrow().cards.entries.len(), 1);
    chat.hide();
    assert_eq!(browse.reads.borrow().cards.entries.len(), 0);
    browse.apply(full_issue(6, 4, ActionState::Draft));
    assert_eq!(browse.reads.borrow().cards.entries.len(), 0);
    browse.disconnected();
    chat.disconnected();
    browse.connected();
    browse.show_for(Some(ThreadId(3)));
    assert_eq!(browse.reads.borrow().cards.entries.len(), 0);
}

#[test]
fn equal_version_hydration_keeps_authority_and_a_conflicting_card_is_terminal() {
    use kumochi_protocol::message::{ActionState, ApprovalRev, StateVersion};
    let mut state = Threads::default();
    state.connected();
    state.show_for(Some(ThreadId(3)));
    assert_eq!(
        state.requests().iter().map(request_id).collect::<Vec<_>>(),
        [1, 2]
    );
    state.apply(issue_page(2, 3));
    assert_eq!(
        state.requests().iter().map(request_id).collect::<Vec<_>>(),
        [3]
    );
    state.action_state(
        DraftId(9),
        StateVersion(4),
        ActionState::Draft,
        Some(ApprovalRev(8)),
        None,
    );
    state.apply(full_issue(3, 4, ActionState::Draft));
    {
        let reads = state.reads.borrow();
        let card = reads.cards.entries[&DraftId(9)].full.as_ref().unwrap();
        assert_eq!(card.approval, Some(ApprovalRev(8)));
        assert_eq!(card.title.as_str(), "private-card-marker");
        assert_eq!(card.hash.0, [7; 32]);
    }
    state.apply(full_issue(3, 4, ActionState::Sending));
    assert!(state.cards_broken());
    state.apply(full_issue(3, 5, ActionState::Draft));
    assert_eq!(
        state.reads.borrow().cards.entries[&DraftId(9)]
            .full
            .as_ref()
            .unwrap()
            .version,
        StateVersion(4)
    );
    assert_eq!(state.requests(), []);
}

#[test]
fn live_review_fills_shared_content_without_releasing_the_history_batch() {
    use kumochi_protocol::message::{ActionState, ApprovalRev, IssueBody, StateVersion};
    let mut cards = cards::Cards::default();
    cards.shown(View::Browse, Some(ThreadId(3)), vec![DraftId(9)]);
    cards.shown(View::Chat, Some(ThreadId(3)), vec![DraftId(9)]);
    cards.start(5, &[DraftId(9)]);
    let DataToShell::IssueCardRead {
        card: mut first, ..
    } = full_issue(5, 1, ActionState::Draft)
    else {
        panic!("card fixture");
    };
    first.approval = Some(ApprovalRev(1));
    cards.full(5, first.clone());
    cards.review_started(DraftId(9), 71);
    let mut review = first.clone();
    review.version = StateVersion(2);
    review.approval = Some(ApprovalRev(2));
    assert!(!cards.review(70, review.clone()));
    assert!(
        cards
            .entry(View::Chat, DraftId(9))
            .unwrap()
            .pending_review()
    );
    assert!(cards.review(71, review.clone()));
    assert!(cards.pending(5));
    cards.full(5, first);
    for view in [View::Browse, View::Chat] {
        let entry = cards.entry(view, DraftId(9)).unwrap();
        assert_eq!(entry.card(), Some(&review));
        assert!(!entry.pending());
        assert!(!entry.loading());
        assert_eq!(entry.feedback, None);
    }
    cards.done(5);
    assert!(!cards.pending(5));
    cards.cancel_review(71, DraftId(9));
    assert!(!cards.review(71, review.clone()));
    cards.review_started(DraftId(9), 72);
    review.body = IssueBody::new("different content at the same version").unwrap();
    assert!(!cards.review(72, review));
    assert!(cards.broken);
}

#[test]
fn a_possible_handoff_lock_survives_queue_failures_and_clear_only_on_new_authority() {
    use kumochi_protocol::message::{ActionState, IssueUnavailableKind, StateVersion};
    let mut cards = cards::Cards::default();
    cards.shown(View::Browse, Some(ThreadId(3)), vec![DraftId(9)]);
    cards.start(5, &[DraftId(9)]);
    let DataToShell::IssueCardRead { card, .. } = full_issue(5, 1, ActionState::Draft) else {
        panic!("card fixture");
    };
    cards.full(5, card);
    cards.done(5);
    cards.submitted(DraftId(9), Some(71));
    cards.unavailable(71, DraftId(9), IssueUnavailableKind::AttemptNotSaved);
    cards.submitted(DraftId(9), None);
    cards.review_started(DraftId(9), 72);
    cards.submitted(DraftId(9), Some(73));
    cards.unavailable(73, DraftId(9), IssueUnavailableKind::Action);
    let entry = cards.entry(View::Browse, DraftId(9)).unwrap();
    assert!(entry.review_blocked());
    assert!(!entry.pending());
    assert_eq!(
        entry.feedback,
        Some(IssueFeedback::Unavailable(
            IssueUnavailableKind::AttemptNotSaved
        ))
    );
    cards.state(DraftId(9), StateVersion(1), ActionState::Draft, None, None);
    assert!(
        cards
            .entry(View::Browse, DraftId(9))
            .unwrap()
            .review_blocked()
    );
    cards.state(
        DraftId(9),
        StateVersion(2),
        ActionState::Unknown {
            lookup: kumochi_protocol::message::LookupResult::None,
            maybe_duplicate: true,
        },
        None,
        None,
    );
    assert!(
        !cards
            .entry(View::Browse, DraftId(9))
            .unwrap()
            .review_blocked()
    );
    assert_eq!(
        cards.entry(View::Browse, DraftId(9)).unwrap().feedback,
        None
    );
}

#[test]
fn eviction_and_failed_reads_do_not_revive_cards_or_create_retry_loops() {
    use kumochi_protocol::message::{ActionState, IssueCardsStatus, StateVersion};
    let mut state = Threads::default();
    state.connected();
    state.show_for(Some(ThreadId(3)));
    assert_eq!(
        state.requests().iter().map(request_id).collect::<Vec<_>>(),
        [1, 2]
    );
    state.apply(issue_page(2, 3));
    assert_eq!(
        state.requests(),
        [DataToBrain::IssueCardsRequest {
            request: 3,
            drafts: Bounded::new(vec![DraftId(9)]).unwrap()
        }]
    );
    state.select(Some(ThreadId(3)));
    assert_eq!(state.reads.borrow().cards.entries.len(), 0);
    assert_eq!(
        state.requests().iter().map(request_id).collect::<Vec<_>>(),
        [4]
    );
    state.apply(issue_page(4, 3));
    assert_eq!(state.requests(), []);
    state.apply(full_issue(3, 99, ActionState::Draft));
    assert!(
        state.reads.borrow().cards.entries[&DraftId(9)]
            .full
            .is_none()
    );
    state.apply(DataToShell::IssueCardsDone {
        request: 3,
        status: IssueCardsStatus::Complete,
    });
    assert_eq!(
        state.requests(),
        [DataToBrain::IssueCardsRequest {
            request: 5,
            drafts: Bounded::new(vec![DraftId(9)]).unwrap()
        }]
    );
    state.apply(DataToShell::IssueCardsDone {
        request: 5,
        status: IssueCardsStatus::Failed,
    });
    assert_eq!(state.requests(), []);
    for id in 100..1100 {
        state.action_state(DraftId(id), StateVersion(1), ActionState::Draft, None, None);
    }
    assert_eq!(state.reads.borrow().cards.entries.len(), 1);
    assert_eq!(
        state.requests().iter().map(request_id).collect::<Vec<_>>(),
        [6]
    );
    assert_eq!(state.requests(), []);
    state.apply(DataToShell::PageFailed { request: 6 });
    assert_eq!(state.requests(), []);
    state.hide();
    assert_eq!(state.reads.borrow().cards.entries.len(), 0);
    state.apply(full_issue(5, 100, ActionState::Draft));
    assert_eq!(state.reads.borrow().cards.entries.len(), 0);
}

#[test]
fn issue_readers_share_authority_but_require_current_display_ownership() {
    use kumochi_protocol::message::{ActionState, IssueCardsStatus, LookupResult, StateVersion};
    let (mut browse, mut chat) = Threads::pair();
    browse.connected();
    browse.show_for(Some(ThreadId(3)));
    chat.connected();
    chat.show_for(Some(ThreadId(4)));
    assert_eq!(
        browse.requests().iter().map(request_id).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        chat.requests().iter().map(request_id).collect::<Vec<_>>(),
        [3, 4]
    );
    browse.apply(issue_page(2, 3));
    assert_eq!(
        browse.requests().iter().map(request_id).collect::<Vec<_>>(),
        [5]
    );
    assert!(browse.issue(DraftId(9)).unwrap().loading());
    assert!(chat.issue(DraftId(9)).is_none());
    assert!(browse.issue(DraftId(999)).is_none());
    browse.apply(full_issue(5, 1, ActionState::Draft));
    assert_eq!(
        browse
            .issue(DraftId(9))
            .unwrap()
            .card()
            .unwrap()
            .body
            .as_str(),
        "private-card-marker"
    );
    browse.action_state(
        DraftId(9),
        StateVersion(2),
        ActionState::Unknown {
            lookup: LookupResult::None,
            maybe_duplicate: true,
        },
        None,
        None,
    );
    assert!(browse.issue(DraftId(9)).unwrap().card().is_none());
    browse.apply(DataToShell::IssueCardsDone {
        request: 5,
        status: IssueCardsStatus::Complete,
    });
    assert_eq!(
        browse.requests().iter().map(request_id).collect::<Vec<_>>(),
        [6]
    );
    browse.apply(DataToShell::IssueCardsDone {
        request: 6,
        status: IssueCardsStatus::Failed,
    });
    {
        let issue = browse.issue(DraftId(9)).unwrap();
        assert!(!issue.loading());
        assert_eq!(
            issue.state(),
            Some(&ActionState::Unknown {
                lookup: LookupResult::None,
                maybe_duplicate: true,
            })
        );
        assert!(issue.card().is_none());
    }
    assert_eq!(browse.requests(), []);
    browse.disconnected();
    assert!(browse.issue(DraftId(9)).is_none());
}

#[test]
fn unknown_issue_repair_replaces_queued_older_read_when_shared_admission_is_full() {
    use kumochi_protocol::message::{ActionState, IssueCardsStatus, StateVersion};
    let (mut browse, mut chat) = Threads::pair();
    browse.connected();
    browse.show_for(Some(ThreadId(3)));
    chat.connected();
    chat.show_for(Some(ThreadId(4)));
    assert_eq!(
        browse.requests().iter().map(request_id).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        chat.requests().iter().map(request_id).collect::<Vec<_>>(),
        [3, 4]
    );
    browse.apply(DataToShell::Page {
        request: 2,
        thread: ThreadId(3),
        events: vec![PageEvent {
            id: EventId(9),
            run: None,
            kind: PageEventKind::Issue { draft: DraftId(9) },
            text: "title".into(),
            cut: false,
        }],
        before: Some(EventId(8)),
    });
    assert_eq!(
        browse.requests(),
        [DataToBrain::IssueCardsRequest {
            request: 5,
            drafts: Bounded::new(vec![DraftId(9)]).unwrap()
        }]
    );
    assert_eq!(browse.reads.borrow().pending.len(), 4);
    browse.older_events();
    assert_eq!(browse.requests(), []);
    assert_eq!(browse.event_needed, Some(Read::More));
    browse.action_state(
        DraftId(999),
        StateVersion(1),
        ActionState::Draft,
        None,
        None,
    );
    assert_eq!(browse.requests(), []);
    assert_eq!(chat.requests(), []);
    browse.apply(DataToShell::IssueCardsDone {
        request: 5,
        status: IssueCardsStatus::Complete,
    });
    assert_eq!(
        browse.requests(),
        [DataToBrain::PageRequest {
            request: 6,
            page: PageOf::Events {
                thread: ThreadId(3),
                before: None
            }
        }]
    );
}

#[test]
fn displayed_ids_are_deduplicated_and_batched_at_sixteen_without_another_scheduler() {
    let mut state = Threads::default();
    state.connected();
    state.show_for(Some(ThreadId(3)));
    assert_eq!(
        state.requests().iter().map(request_id).collect::<Vec<_>>(),
        [1, 2]
    );
    let mut events: Vec<_> = (1..=17)
        .map(|id| PageEvent {
            id: EventId(id),
            run: None,
            kind: PageEventKind::Issue { draft: DraftId(id) },
            text: "title".into(),
            cut: false,
        })
        .collect();
    events.insert(1, events[0].clone());
    state.apply(DataToShell::Page {
        request: 2,
        thread: ThreadId(3),
        events,
        before: None,
    });
    assert_eq!(
        state.requests(),
        [
            DataToBrain::IssueCardsRequest {
                request: 3,
                drafts: Bounded::new((1..=16).map(DraftId).collect()).unwrap()
            },
            DataToBrain::IssueCardsRequest {
                request: 4,
                drafts: Bounded::new(vec![DraftId(17)]).unwrap()
            },
        ]
    );
    assert_eq!(state.requests(), []);
    assert_eq!(state.reads.borrow().cards.entries.len(), 17);
}

#[test]
fn cards_and_pages_share_capacity_but_only_their_matching_terminals_release_it() {
    use kumochi_protocol::message::IssueCardsStatus;
    let (mut browse, mut chat) = Threads::pair();
    browse.connected();
    browse.show();
    chat.connected();
    chat.show();
    assert_eq!(
        browse.requests().iter().map(request_id).collect::<Vec<_>>(),
        [1]
    );
    assert_eq!(
        chat.requests().iter().map(request_id).collect::<Vec<_>>(),
        [2]
    );
    assert_eq!(
        browse.card_request(Bounded::default()),
        Some(DataToBrain::IssueCardsRequest {
            request: 3,
            drafts: Bounded::default()
        })
    );
    assert_eq!(
        chat.card_request(Bounded::default()),
        Some(DataToBrain::IssueCardsRequest {
            request: 4,
            drafts: Bounded::default()
        })
    );
    assert_eq!(browse.card_request(Bounded::default()), None);
    browse.apply(DataToShell::IssueCardsDone {
        request: 1,
        status: IssueCardsStatus::Complete,
    });
    browse.apply(DataToShell::PageFailed { request: 3 });
    chat.apply(DataToShell::IssueCardsDone {
        request: 3,
        status: IssueCardsStatus::Failed,
    });
    assert_eq!(
        browse.reads.borrow().pending,
        [
            (1, View::Browse),
            (2, View::Chat),
            (3, View::Browse),
            (4, View::Chat)
        ]
    );
    browse.apply(DataToShell::IssueCardsDone {
        request: 3,
        status: IssueCardsStatus::Failed,
    });
    assert_eq!(
        browse.card_request(Bounded::default()),
        Some(DataToBrain::IssueCardsRequest {
            request: 5,
            drafts: Bounded::default()
        })
    );
    browse.apply(DataToShell::IssueCardsDone {
        request: 3,
        status: IssueCardsStatus::Complete,
    });
    assert_eq!(browse.card_request(Bounded::default()), None);
    chat.disconnected();
    browse.disconnected();
    browse.connected();
    browse.show();
    browse.apply(DataToShell::IssueCardsDone {
        request: 5,
        status: IssueCardsStatus::Complete,
    });
    assert_eq!(
        browse.card_request(Bounded::default()),
        Some(DataToBrain::IssueCardsRequest {
            request: 6,
            drafts: Bounded::default()
        })
    );
}

#[test]
fn terminal_kind_must_match_before_either_pending_page_slot_is_released() {
    for events_first in [false, true] {
        let mut state = Threads::default();
        state.connected();
        state.show_for(Some(ThreadId(3)));
        let requests = state.requests();
        assert_eq!(requests.iter().map(request_id).collect::<Vec<_>>(), [1, 2]);
        state.apply(reply(2, &[99], None));
        state.apply(DataToShell::Page {
            request: 1,
            thread: ThreadId(3),
            events: vec![event(99)],
            before: None,
        });
        assert_eq!(
            state.reads.borrow().pending,
            [(1, View::Browse), (2, View::Browse)]
        );
        assert_eq!(state.list_pending.map(|pending| pending.request), Some(1));
        assert_eq!(state.event_pending.map(|pending| pending.request), Some(2));
        assert_eq!(state.rows, []);
        assert_eq!(state.events, []);
        assert!(!state.listed);
        assert!(!state.loaded);
        assert_eq!(state.requests(), []);
        let mut terminals = vec![
            reply(1, &[3], None),
            DataToShell::Page {
                request: 2,
                thread: ThreadId(3),
                events: vec![event(42)],
                before: None,
            },
        ];
        if events_first {
            terminals.reverse();
        }
        for terminal in &terminals {
            state.apply(terminal.clone());
        }
        assert_eq!(state.reads.borrow().pending, []);
        assert_eq!(state.list_pending.map(|pending| pending.request), None);
        assert_eq!(state.event_pending.map(|pending| pending.request), None);
        assert_eq!(state.rows, [row(3)]);
        assert_eq!(state.events, [event(42)]);
        assert!(state.listed);
        assert!(state.loaded);
        state.refresh();
        assert_eq!(state.requests(), [list_request(3)]);
        for terminal in terminals {
            state.apply(terminal);
        }
        state.apply(DataToShell::PageFailed { request: 2 });
        assert_eq!(state.reads.borrow().pending, [(3, View::Browse)]);
        assert!(!state.failed);
        state.apply(DataToShell::PageFailed { request: 3 });
        state.apply(DataToShell::PageFailed { request: 3 });
        assert_eq!(state.reads.borrow().pending, []);
        assert!(state.failed);
        assert_eq!(state.requests(), []);
    }
}

#[test]
fn only_a_repair_issued_after_committed_done_proves_the_stored_answer() {
    let mut state = Threads::chat();
    state.connected();
    state.show_for(Some(ThreadId(3)));
    let first = state.requests();
    state.settled(ThreadId(3), RunId(1));
    state.run_state(ThreadId(3));
    state.apply(DataToShell::Page {
        request: request_id(&first[1]),
        thread: ThreadId(3),
        events: vec![event(1)],
        before: None,
    });
    assert_eq!(state.proven, None, "the request preceded Done");
    let repair = state.requests();
    assert_eq!(repair.len(), 1);
    let mut early = event(2);
    early.kind = PageEventKind::AssistantText;
    early.text = "early round".into();
    let mut late = event(3);
    late.kind = PageEventKind::AssistantText;
    late.text = "late round".into();
    let stored = vec![late, early, event(1)];
    state.apply(DataToShell::Page {
        request: request_id(&repair[0]),
        thread: ThreadId(3),
        events: stored.clone(),
        before: None,
    });
    assert_eq!(state.proven, Some(RunId(1)));
    assert_eq!(state.events, stored);
    state.run_state(ThreadId(3));
    let next = state.requests();
    state.apply(DataToShell::Page {
        request: request_id(&next[0]),
        thread: ThreadId(3),
        events: stored.clone(),
        before: None,
    });
    assert_eq!(
        state.events, stored,
        "distinct rows survive another repair exactly once"
    );
    assert_eq!(state.proven, Some(RunId(1)));
}

#[test]
fn an_older_page_cannot_replace_a_failed_post_done_repair_as_proof() {
    let mut state = Threads::chat();
    state.connected();
    state.show_for(Some(ThreadId(3)));
    let first = state.requests();
    state.apply(DataToShell::Page {
        request: request_id(&first[1]),
        thread: ThreadId(3),
        events: vec![event(8)],
        before: Some(EventId(8)),
    });
    state.settled(ThreadId(3), RunId(1));
    state.run_state(ThreadId(3));
    let repair = state.requests();
    state.apply(DataToShell::PageFailed {
        request: request_id(&repair[0]),
    });
    state.older_events();
    let older = state.requests();
    state.apply(DataToShell::Page {
        request: request_id(&older[0]),
        thread: ThreadId(3),
        events: vec![event(7)],
        before: None,
    });
    assert_eq!(state.proven, None);
    assert_eq!(state.events, [event(8), event(7)]);
}

#[test]
fn selection_and_connection_lifetimes_fence_stored_answer_proof() {
    let mut state = Threads::chat();
    state.connected();
    state.show_for(Some(ThreadId(3)));
    state.settled(ThreadId(3), RunId(1));
    let first = state.requests();
    state.select(Some(ThreadId(4)));
    state.apply(DataToShell::Page {
        request: request_id(&first[1]),
        thread: ThreadId(3),
        events: vec![event(1)],
        before: None,
    });
    assert_eq!(state.proven, None);
    assert_eq!(state.events, []);
    state.settled(ThreadId(3), RunId(2));
    assert_eq!(
        state.settled, None,
        "another thread cannot mark this selection committed"
    );
    state.settled(ThreadId(4), RunId(2));
    state.proven = Some(RunId(2));
    state.disconnected();
    assert_eq!((state.settled, state.proven), (None, None));
    state.connected();
    assert_eq!((state.settled, state.proven), (None, None));
}

#[test]
fn chat_follows_the_input_target_not_the_browsed_task() {
    let mut runs = crate::runs::Runs::default();
    runs.settings(ThreadId(1));
    assert!(runs.select_thread(Some(ThreadId(3))));
    let mut panel = Threads::default();
    panel.select(Some(ThreadId(4)));
    let mut chat = Threads::chat();
    chat.show_for(runs.thread());
    assert_eq!(chat.open, Some(ThreadId(3)));
    panel.select(Some(ThreadId(5)));
    assert!(!chat.follow(runs.thread()));
    assert_eq!(panel.open, Some(ThreadId(5)));
    assert_eq!(chat.open, Some(ThreadId(3)));
    runs.back_to_kumo();
    assert!(chat.follow(runs.thread()));
    assert_eq!(chat.open, Some(ThreadId(1)));
    assert!(runs.select_thread(None));
    assert!(chat.follow(runs.thread()));
    assert_eq!(chat.open, None);
    assert_eq!(panel.open, Some(ThreadId(5)));
}

#[test]
fn panel_and_chat_allocate_disjoint_ids_through_hide_and_reconnect() {
    let (mut panel, mut chat) = Threads::pair();
    for state in [&mut panel, &mut chat] {
        state.show_for(Some(ThreadId(3)));
        state.connected();
    }
    let panel_requests = panel.requests();
    let chat_requests = chat.requests();
    assert_eq!(
        panel_requests.iter().map(request_id).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        chat_requests.iter().map(request_id).collect::<Vec<_>>(),
        [3, 4]
    );
    for request in 1..=4 {
        assert_eq!(panel.owns_request(request), request <= 2);
        assert_eq!(chat.owns_request(request), request >= 3);
    }
    panel.hide();
    chat.hide();
    for (state, requests) in [(&mut panel, panel_requests), (&mut chat, chat_requests)] {
        for request in requests {
            state.apply(DataToShell::PageFailed {
                request: request_id(&request),
            });
        }
        state.show_for(Some(ThreadId(3)));
    }
    assert_eq!(
        panel.requests().iter().map(request_id).collect::<Vec<_>>(),
        [5, 6]
    );
    assert_eq!(
        chat.requests().iter().map(request_id).collect::<Vec<_>>(),
        [7, 8]
    );
    panel.disconnected();
    chat.disconnected();
    panel.connected();
    chat.connected();
    assert_eq!(
        panel.requests().iter().map(request_id).collect::<Vec<_>>(),
        [9, 10]
    );
    assert_eq!(
        chat.requests().iter().map(request_id).collect::<Vec<_>>(),
        [11, 12]
    );
}

#[test]
fn request_wrap_preserves_both_owners_parity() {
    let (mut panel, mut chat) = Threads::pair();
    panel.reads.borrow_mut().counter = u32::MAX - 1;
    panel.show_for(Some(ThreadId(3)));
    chat.show_for(Some(ThreadId(3)));
    panel.connected();
    chat.connected();
    assert_eq!(
        panel.requests().iter().map(request_id).collect::<Vec<_>>(),
        [u32::MAX]
    );
    assert_eq!(chat.requests(), []);
    assert!(panel.exhausted());
    assert!(chat.exhausted());
    assert!(panel.owns_request(u32::MAX));
    assert!(!chat.owns_request(u32::MAX));
    assert!(!panel.owns_request(0));
    panel.disconnected();
    chat.disconnected();
    panel.connected();
    assert_eq!(
        panel.requests().iter().map(request_id).collect::<Vec<_>>(),
        [1, 2]
    );
}

#[test]
fn reopening_the_same_focus_reads_first_not_older_events() {
    let mut state = Threads::chat();
    state.connected();
    state.show_for(Some(ThreadId(3)));
    let requests = state.requests();
    state.apply(reply(request_id(&requests[0]), &[3], None));
    state.apply(DataToShell::Page {
        request: 2,
        thread: ThreadId(3),
        events: vec![event(8)],
        before: Some(EventId(8)),
    });
    assert!(!state.follow(Some(ThreadId(3))));
    assert_eq!(state.events, [event(8)]);
    assert_eq!(state.requests(), []);
    state.hide();
    assert_eq!(state.requests(), []);
    state.show_for(Some(ThreadId(3)));
    assert_eq!(state.open, Some(ThreadId(3)));
    assert_eq!(state.events, []);
    assert_eq!(state.before, None);
    assert!(!state.loaded);
    assert_eq!(
        state.requests(),
        [
            list_request(3),
            DataToBrain::PageRequest {
                request: 4,
                page: PageOf::Events {
                    thread: ThreadId(3),
                    before: None
                },
            },
        ]
    );
}

#[test]
fn both_reducers_hold_exactly_four_slots_until_their_stale_pages_land() {
    let (mut panel, mut chat) = Threads::pair();
    panel.connected();
    chat.connected();
    panel.show_for(Some(ThreadId(3)));
    chat.show_for(Some(ThreadId(3)));
    let panel_requests = panel.requests();
    let chat_requests = chat.requests();
    assert_eq!(
        panel_requests.iter().map(request_id).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        chat_requests.iter().map(request_id).collect::<Vec<_>>(),
        [3, 4]
    );
    panel.search("panel search");
    chat.search("chat search");
    assert!(panel.follow(Some(ThreadId(2))));
    assert!(chat.follow(Some(ThreadId(1))));
    panel.changed();
    chat.changed();
    assert_eq!(panel.requests(), []);
    assert_eq!(chat.requests(), []);
    for (request, thread) in [(2, ThreadId(3)), (4, ThreadId(3))] {
        let owner = if panel.owns_request(request) {
            &mut panel
        } else {
            &mut chat
        };
        owner.apply(DataToShell::Page {
            request,
            thread,
            events: vec![event(99)],
            before: None,
        });
        assert_eq!(owner.events, []);
        assert!(!owner.loaded);
    }
    panel.apply(reply(1, &[99], None));
    chat.apply(reply(3, &[99], None));
    assert_eq!(panel.rows, []);
    assert_eq!(chat.rows, []);
    assert_eq!(
        panel.requests(),
        [
            DataToBrain::PageRequest {
                request: 5,
                page: PageOf::Threads {
                    search: Some("panel search".to_owned()),
                    after: None
                }
            },
            DataToBrain::PageRequest {
                request: 6,
                page: PageOf::Events {
                    thread: ThreadId(2),
                    before: None
                }
            },
        ]
    );
    assert_eq!(
        chat.requests(),
        [
            DataToBrain::PageRequest {
                request: 7,
                page: PageOf::Threads {
                    search: Some("chat search".to_owned()),
                    after: None
                }
            },
            DataToBrain::PageRequest {
                request: 8,
                page: PageOf::Events {
                    thread: ThreadId(1),
                    before: None
                }
            },
        ]
    );
    assert_eq!(panel.requests(), []);
    assert_eq!(chat.requests(), []);
}

#[test]
fn delete_and_committed_rename_fanout_update_both_reducers() {
    for mut state in [Threads::default(), Threads::chat()] {
        state.connected();
        state.show_for(Some(ThreadId(3)));
        let requests = state.requests();
        state.apply(reply(request_id(&requests[0]), &[3, 2], None));
        state.renamed(ThreadId(2), "committed title");
        let rename = state.requests().remove(0);
        state.changed();
        state.apply(reply(request_id(&rename), &[3, 2], None));
        let committed_read = state.requests().remove(0);
        let mut committed = row(2);
        committed.title = "committed title".to_owned();
        state.apply(DataToShell::ThreadList {
            request: request_id(&committed_read),
            threads: vec![row(3), committed.clone()],
            next: None,
        });
        assert_eq!(state.row(ThreadId(2)), Some(&committed));
        state.deleted(ThreadId(3));
        state.apply(DataToShell::Page {
            request: request_id(&requests[1]),
            thread: ThreadId(3),
            events: vec![event(99)],
            before: None,
        });
        assert_eq!(state.open, None);
        assert_eq!(state.events, []);
        assert_eq!(state.rows, [committed]);
        assert_eq!(state.row(ThreadId(3)), None);
    }
}

fn settings(name: &str) -> SettingsView {
    SettingsView {
        assistant_name: kumochi_protocol::message::AssistantName::new(name).expect("valid"),
        user_name: None,
        onboarded: true,
        onboarding_from: kumochi_protocol::message::OnboardingStep::Welcome,
        main_thread: ThreadId(1),
        max_frame_rate: kumochi_protocol::message::MaxFrameRate::AUTOMATIC,
        default_model: None,
        dock_position: kumochi_protocol::message::DockPosition::TopCenter,
        show_in_taskbar: false,
        talk_back: false,
        stt_unload: kumochi_protocol::message::SttUnload::Kept,
        hold_to_talk: kumochi_protocol::message::DEFAULT_HOLD_TO_TALK,
        screen_holds: kumochi_protocol::message::DEFAULT_SCREEN_HOLDS,
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
    state.renamed(ThreadId(1), "confirmed new title");
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
    state.renamed(ThreadId(1), "confirmed new title");
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
    state.renamed(ThreadId(3), "confirmed new title");
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
    state.renamed(ThreadId(1), "confirmed new title");
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
    state.renamed(ThreadId(1), "confirmed new title");
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
    state.renamed(ThreadId(1), "confirmed new title");
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
    state.renamed(ThreadId(1), "confirmed new title");
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
    state.renamed(ThreadId(1), "confirmed new title");
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
    match request {
        DataToBrain::PageRequest { request, .. }
        | DataToBrain::IssueCardsRequest { request, .. } => *request,
        _ => panic!("page or issue card request"),
    }
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
        request: 49,
        thread: ThreadId(3),
        events: vec![event(4)],
        before: None,
    };
    assert_eq!(state.receive(unsolicited, 321), 321);
    assert!(state.events.is_empty());
    assert_eq!(
        state.receive(DataToShell::PageFailed { request: 49 }, 17),
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

/// M135-1: the reread of a renamed task's page can beat the rename's commit across the two pipes.
/// The committed-title notification then reads that page again, whether it arrives before the
/// stale reply or after it.
#[test]
fn an_offpage_rename_shows_its_committed_title_whichever_side_of_the_stale_reply_its_notice_lands()
{
    let second_page = Some(ThreadCursor {
        updated_ms: 2,
        id: ThreadId(2),
        rank_bits: 0,
    });
    for notice_first in [true, false] {
        let mut state = with_second_page();
        state.renamed(ThreadId(1), "committed title");
        let old_page = state.requests().remove(0);
        assert!(matches!(
            &old_page,
            DataToBrain::PageRequest { page: PageOf::Threads { after, .. }, .. } if *after == second_page
        ));
        if notice_first {
            state.changed();
        }
        state.apply(reply(request_id(&old_page), &[1, 0], None));
        assert_eq!(
            state.rows[2].title, "private title 1",
            "read before the commit"
        );
        if !notice_first {
            state.changed();
        }
        let again = state.requests().remove(0);
        assert!(
            matches!(
                &again,
                DataToBrain::PageRequest { page: PageOf::Threads { after, .. }, .. } if *after == second_page
            ),
            "the renamed task's own page, not page one: {again:?}"
        );
        let mut committed = row(1);
        committed.title = "committed title".to_owned();
        state.apply(DataToShell::ThreadList {
            request: request_id(&again),
            threads: vec![committed.clone(), row(0)],
            next: None,
        });
        assert_eq!(state.rows[2], committed);
        assert_eq!(state.row(ThreadId(1)), Some(&committed));
        while let Some(request) = state.requests().pop() {
            state.apply(reply(request_id(&request), &[3, 2], Some(2)));
        }
        assert_eq!(state.rows[2], committed, "page one's repair keeps it");

        state.changed();
        let requests = state.requests();
        assert!(
            matches!(
                requests.as_slice(),
                [DataToBrain::PageRequest {
                    page: PageOf::Threads { after: None, .. },
                    ..
                }]
            ),
            "a confirmed rename isn't read again: {requests:?}"
        );
    }
}
