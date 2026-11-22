use crate::link::{LinkHandle, Session};
use crate::presence::issue_review::machine::{Effect, Reviews, Route};
use crate::presence::messages::consent::machine::{Facts, Opening, Pending, Window};
use crate::settings::Settings;
use crate::threads::Threads;
use async_channel::Receiver;
use kumochi_protocol::message::{
    ActionState, ApprovalRev, ContentHash, ControlToBrain, DataToBrain, DataToShell, Decision,
    DraftId, EventId, FailReason, GitHubLogin, GitHubView, Incarnation, IssueBody, IssueCard,
    IssueCardsStatus, IssueOp, IssueSession, IssueTitle, PageEvent, PageEventKind, RepoName,
    StateVersion, ThreadId,
};
use kumochi_scene::text::Bounded;
use std::time::Instant;

mod reviews;

const SHOWN: Window = Window {
    active: true,
    visible: true,
};

/// The owner's review route and both histories' consent intents, joined as the shell joins them.
struct Fixture {
    route: Option<Route<usize>>,
    request: u64,
    session: Session,
    link: LinkHandle,
    controls: Receiver<ControlToBrain>,
    settings: Settings,
    models: [Threads; 2],
    pending: [Option<Pending<()>>; 2],
}

fn connected() -> GitHubView {
    GitHubView::Connected {
        login: GitHubLogin::new("octo").unwrap(),
        repos: Bounded::new(vec![
            RepoName::new("o/r").unwrap(),
            RepoName::new("o/other").unwrap(),
        ])
        .unwrap(),
        default: Some(RepoName::new("o/other").unwrap()),
        truncated: false,
    }
}

fn page(request: u32) -> DataToShell {
    DataToShell::Page {
        request,
        thread: ThreadId(1),
        before: None,
        events: vec![PageEvent {
            id: EventId(7),
            run: None,
            kind: PageEventKind::Issue { draft: DraftId(9) },
            text: "old title".into(),
            cut: false,
        }],
    }
}

fn fixture(state: ActionState) -> Fixture {
    let (mut link, _requests) = LinkHandle::recording();
    let controls = link.record_controls();
    let mut settings = Settings::default();
    assert!(
        settings
            .github
            .receive(Some(1), 1, connected(), Instant::now())
    );
    let (mut browse, mut chat) = Threads::pair();
    for model in [&mut browse, &mut chat] {
        model.connected();
        model.show_for(Some(ThreadId(1)));
        assert_eq!(model.requests().len(), 2);
    }
    browse.apply_for_tests(page(2));
    assert_eq!(
        browse.requests(),
        [DataToBrain::IssueCardsRequest {
            request: 5,
            drafts: Bounded::new(vec![DraftId(9)]).unwrap(),
        }]
    );
    chat.apply_for_tests(page(4));
    browse.apply_for_tests(DataToShell::IssueCardRead {
        request: 5,
        card: IssueCard {
            draft: DraftId(9),
            thread: ThreadId(1),
            repo: Some(RepoName::new("o/r").unwrap()),
            account: Some(GitHubLogin::new("octo").unwrap()),
            title: IssueTitle::new("saved title").unwrap(),
            body: IssueBody::new("saved body").unwrap(),
            hash: ContentHash([7; 32]),
            approval: Some(ApprovalRev(1)),
            version: StateVersion(1),
            state,
        },
    });
    browse.apply_for_tests(DataToShell::IssueCardsDone {
        request: 5,
        status: IssueCardsStatus::Complete,
    });
    Fixture {
        route: None,
        request: 0,
        session: Session::detached(1, Incarnation(1)),
        link,
        controls,
        settings,
        models: [browse, chat],
        pending: [None, None],
    }
}

impl Fixture {
    fn facts(&self, at: usize) -> Facts<'_> {
        Facts {
            model: &self.models[at],
            connection: Some((&self.session, &self.settings)),
        }
    }

    /// One review decision, with the view's retirement run afterwards as the shell defers it.
    fn review<R>(&mut self, decide: impl FnOnce(&mut Reviews<'_, usize>) -> R) -> R {
        let mut retired = Vec::new();
        let model = &mut self.models[0];
        let decided = decide(&mut Reviews {
            route: &mut self.route,
            request: &mut self.request,
            session: Some(&self.session),
            link: &self.link,
            apply: &mut |effect| match effect {
                Effect::Threads(update) => update.apply(model),
                Effect::RetireView(at, review) => retired.push((at, review)),
            },
        });
        for (at, review) in retired {
            if self.pending[at]
                .as_ref()
                .is_some_and(|pending| pending.review() == review)
            {
                self.retire(at);
            }
        }
        decided
    }

    fn retire(&mut self, at: usize) {
        if let Some(pending) = self.pending[at].take() {
            let (review, _dialog) = pending.retire();
            self.review(|reviews| reviews.cancel_review(review));
        }
    }

    fn click(&mut self, at: usize) -> u64 {
        let opening = Opening::new(
            self.pending[at].as_ref(),
            DraftId(9),
            &SHOWN,
            false,
            &self.facts(at),
        )
        .expect("the card is eligible");
        let review = self
            .review(|reviews| reviews.begin(opening.rearm(), at, opening.live()))
            .expect("the rearm was queued");
        self.pending[at] = Some(opening.pending(review));
        review
    }

    fn reply(&mut self, request: u64, card: IssueCard) {
        let Some((at, _session, live)) =
            self.review(|reviews| reviews.deliver(request, card.draft))
        else {
            return;
        };
        if !(live.get() && self.receive(at, request, card)) {
            self.review(|reviews| reviews.cancel_review(request));
        }
    }

    fn receive(&mut self, at: usize, request: u64, card: IssueCard) -> bool {
        let Some(mut pending) = self.pending[at].take() else {
            return false;
        };
        if !pending.awaits(request) {
            self.pending[at] = Some(pending);
            return false;
        }
        let refused = pending.refuses(&card, &SHOWN, &self.facts(at));
        let (version, approval) = (card.version, card.approval);
        if refused
            || !self.review(|reviews| reviews.accepts(request))
            || !self.models[0].issue_review(request, card)
        {
            self.pending[at] = Some(pending);
            self.retire(at);
            return false;
        }
        pending.offer(&mut self.models[at], version, approval.unwrap(), ());
        self.pending[at] = Some(pending);
        true
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the confirmation callback supplies a boolean decision"
    )]
    fn decide(&mut self, at: usize, confirmed: bool, window: &Window) {
        let Some(pending) = self.pending[at].take() else {
            return;
        };
        let consent = pending.consent(confirmed, window, &self.facts(at));
        let Some((review, op)) = consent else {
            self.pending[at] = Some(pending);
            self.retire(at);
            return;
        };
        self.review(|reviews| reviews.confirm(review, op));
    }

    fn escape(&mut self, at: usize) {
        if self.pending[at].as_ref().is_some_and(Pending::awaiting) {
            self.retire(at);
        }
    }

    /// Clicks File in history `at` and answers its rearm with the next approval.
    fn open(&mut self, at: usize) {
        let mut card = self.models[at]
            .issue(DraftId(9))
            .unwrap()
            .card()
            .unwrap()
            .clone();
        let review = self.click(at);
        assert_eq!(
            self.controls.try_recv().unwrap(),
            issue(
                review,
                IssueOp::Rearm {
                    draft: DraftId(9),
                    version: card.version,
                    hash: card.hash,
                }
            )
        );
        card.version.0 += 1;
        card.approval = Some(ApprovalRev(
            card.approval.map_or(1, |approval| approval.0 + 1),
        ));
        self.reply(review, card);
        assert!(
            self.pending[at]
                .as_ref()
                .is_some_and(|pending| !pending.awaiting())
        );
    }
}

fn issue(request: u64, op: IssueOp) -> ControlToBrain {
    ControlToBrain::Issue {
        session: IssueSession {
            incarnation: Incarnation(1),
            epoch: 1,
        },
        request,
        op: Box::new(op),
    }
}

fn cancelled(f: &Fixture, request: u64, review: u64) {
    assert_eq!(
        f.controls.try_recv().unwrap(),
        issue(
            request,
            IssueOp::CancelReview {
                draft: DraftId(9),
                review,
            }
        )
    );
    assert!(f.controls.try_recv().is_err());
}

fn payload(request: u64, approval: u64) -> ControlToBrain {
    issue(
        request,
        IssueOp::Consent {
            draft: DraftId(9),
            hash: ContentHash([7; 32]),
            approval: ApprovalRev(approval),
            decision: Decision::File,
        },
    )
}

#[test]
fn draft_and_definite_failure_emit_only_the_exact_decision_and_never_a_remote_outcome() {
    for state in [ActionState::Draft, ActionState::Failed(FailReason::Network)] {
        let mut f = fixture(state.clone());
        f.open(0);
        f.decide(0, true, &SHOWN);
        assert_eq!(f.controls.try_recv().unwrap(), payload(2, 2));
        f.decide(0, true, &SHOWN);
        assert!(f.controls.try_recv().is_err());
        for model in &f.models {
            assert_eq!(model.issue(DraftId(9)).unwrap().state(), Some(&state));
        }
        f.models[0].action_state(
            DraftId(9),
            StateVersion(2),
            state.clone(),
            Some(ApprovalRev(2)),
            Some(2),
        );
        f.open(1);
        f.decide(1, true, &SHOWN);
        assert_eq!(f.controls.try_recv().unwrap(), payload(4, 3));
        assert!(f.controls.try_recv().is_err());
    }
}

#[test]
fn cancel_escape_immediate_and_held_enter_retire_reviews_without_filing() {
    let mut f = fixture(ActionState::Draft);
    f.open(0);
    f.decide(0, false, &SHOWN);
    cancelled(&f, 2, 1);
    assert!(f.pending[0].is_none());
    f.decide(0, true, &SHOWN);
    assert!(f.controls.try_recv().is_err());
    // A yes from a window that is no longer active or shown refuses as well.
    let lost = [
        Window {
            active: false,
            ..SHOWN
        },
        Window {
            visible: false,
            ..SHOWN
        },
    ];
    for (window, request, review) in [(&lost[0], 4, 3), (&lost[1], 6, 5)] {
        f.open(0);
        f.decide(0, true, window);
        cancelled(&f, request, review);
        assert!(f.pending[0].is_none());
    }
    f.open(0);
    f.decide(0, true, &SHOWN);
    assert_eq!(f.controls.try_recv().unwrap(), payload(8, 5));
}
