use std::num::NonZeroU64;
use std::time::Duration;

use async_channel::{Receiver, TryRecvError};
use kumochi_protocol::geometry::{PhysicalPoint, PhysicalRect};
use kumochi_protocol::message::{Incarnation, Phase, RunOrigin, TypedScreenProof};

use crate::presence::exclusion::machine::fixture::{EXCLUDED, Fake, PRIOR, Prior};

use super::{
    CaptureFor, ControlToBrain, DataToBrain, ErrorKey, Exclusion, Instant, LinkEvent, LinkHandle,
    Refused, ReleasePointer, RunId, Runs, ScreenChoice, Session, ThreadId, TypedDisplay,
    TypedScreenInput, TypedScreenIo, TypedScreenOffer, TypedScreenSubmit,
};

const WORDS: &str = "typed-screen-sentinel-e35d";
const OFFER: TypedScreenOffer = TypedScreenOffer(NonZeroU64::MIN);
const MAIN: ThreadId = ThreadId(1);
const DISPLAY: TypedDisplay = TypedDisplay {
    id: NonZeroU64::MIN,
    bounds: PhysicalRect {
        origin: [-100, 0],
        size: [100, 100],
    },
    dpi: 96,
};

struct Fixture {
    runs: Runs,
    link: LinkHandle,
    session: Session,
    data: Receiver<DataToBrain>,
    control: Receiver<ControlToBrain>,
    exclusion: Exclusion<Prior>,
    surfaces: Fake,
    at: Instant,
}

impl Fixture {
    fn new(thread: Option<ThreadId>) -> Self {
        let (mut link, session, data) = LinkHandle::record_typed_screen();
        let control = link.record_controls_bounded();
        let mut runs = Runs::default();
        runs.met(Incarnation(1));
        runs.connected(1);
        runs.settings(MAIN);
        assert!(runs.select_thread(thread), "fixture thread is available");
        runs.typed_draft_changed();
        Self {
            runs,
            link,
            session,
            data,
            control,
            exclusion: Exclusion::default(),
            surfaces: Fake::new(),
            at: Instant::now(),
        }
    }

    fn arm(&mut self, turn: u32) {
        assert!(
            self.runs
                .request_typed_screen(&self.link, &self.session, DISPLAY),
            "paired and exact"
        );
        let thread = self.runs.thread();
        let draft = self.runs.typed_draft();
        assert_eq!(
            self.control.try_recv(),
            Ok(ControlToBrain::RequestTypedScreenOffer {
                turn,
                thread,
                draft,
                display: Box::new(DISPLAY)
            })
        );
        assert!(
            self.runs.typed_screen_offered(
                &self.link,
                &self.session,
                &LinkEvent::TypedScreenOffered {
                    generation: 1,
                    turn,
                    thread,
                    draft,
                    offer: Some(OFFER),
                }
            ),
            "only the exact offer arms"
        );
    }

    fn send(&mut self, display: TypedDisplay) -> Result<(), Refused> {
        let mut io = TypedScreenIo {
            link: &self.link,
            session: &self.session,
            exclusion: &mut self.exclusion,
            surfaces: &mut self.surfaces,
        };
        self.runs.submit_typed_screen(
            WORDS,
            TypedScreenInput {
                display,
                point: ReleasePointer(PhysicalPoint([-1, 1])),
                submitted_qpc: 10,
                now: self.at,
            },
            &mut io,
            || Some(11),
        )
    }

    fn queued_picture(&mut self) -> TypedScreenSubmit {
        let DataToBrain::TextSubmit {
            turn,
            screen: ScreenChoice::Typed(picture),
            thread,
            text,
        } = self.data.try_recv().expect("picture submit")
        else {
            panic!("only a typed submit owns the picture");
        };
        assert_eq!(turn, 1);
        assert_eq!(thread, self.runs.thread());
        assert_eq!(text, WORDS);
        assert_eq!(picture.display, DISPLAY);
        assert_eq!((picture.submitted_qpc, picture.excluded_qpc), (10, 11));
        assert_eq!(picture.draft, self.runs.typed_draft());
        picture
    }

    fn held(&mut self, generation: u64, proof: TypedScreenProof, now: Instant) -> bool {
        let mut io = TypedScreenIo {
            link: &self.link,
            session: &self.session,
            exclusion: &mut self.exclusion,
            surfaces: &mut self.surfaces,
        };
        self.runs
            .confirm_typed_screen(
                &LinkEvent::TypedScreenHeld { generation, proof },
                Some(DISPLAY),
                now,
                &mut io,
            )
            .expect("fixture restores")
    }

    fn flush(&mut self) {
        let mut io = TypedScreenIo {
            link: &self.link,
            session: &self.session,
            exclusion: &mut self.exclusion,
            surfaces: &mut self.surfaces,
        };
        self.runs
            .flush_typed_screen_cancel(&mut io)
            .expect("fixture restores");
    }

    fn retry(&mut self) -> Result<(), Refused> {
        self.runs.retry_typed_screen(&self.link, &self.session)
    }

    fn assert_plain(&self, turn: u32, thread: Option<ThreadId>) {
        assert_eq!(
            self.data.try_recv(),
            Ok(DataToBrain::TextSubmit {
                turn,
                screen: ScreenChoice::None,
                thread,
                text: WORDS.into(),
            })
        );
    }

    fn assert_cancel(&self) {
        assert_eq!(
            self.control.try_recv(),
            Ok(ControlToBrain::CancelTypedScreen {
                turn: 1,
                offer: OFFER
            })
        );
    }
}

fn proof(picture: TypedScreenSubmit) -> TypedScreenProof {
    TypedScreenProof {
        turn: 1,
        offer: OFFER,
        lease: picture.lease,
        capture: NonZeroU64::new(3).expect("capture identity"),
    }
}

#[test]
fn pre_run_refusal_keeps_new_draft_words_and_plain_retry_spends_only_on_enqueue() {
    let mut f = Fixture::new(None);
    f.arm(1);
    assert_eq!(f.send(DISPLAY), Ok(()));
    let picture = f.queued_picture();
    f.runs.refused(1, Some(MAIN));
    f.flush();
    assert_eq!(f.retry(), Err(Refused::Busy));
    assert_eq!(f.surfaces.affinities(), [EXCLUDED; 8]);
    f.runs.refused(1, None);
    f.flush();
    assert_eq!(
        f.control.try_recv(),
        Err(TryRecvError::Empty),
        "Brain already settled"
    );
    assert_eq!(f.surfaces.affinities(), PRIOR);
    assert!(!f.held(1, proof(picture), f.at), "refusal stays final");
    assert_eq!(
        f.runs
            .submit(WORDS, |_| panic!("recovery needs an explicit plain retry")),
        Err(Refused::Busy)
    );
    assert!(
        f.link
            .send_data(&f.session, DataToBrain::Credit { bytes: 9 }),
        "fill local queue"
    );
    assert_eq!(f.retry(), Err(Refused::Offline));
    let debug = format!("{:?}", f.runs);
    assert!(!debug.contains(WORDS), "words cannot enter diagnostics");
    assert!(
        !debug.contains(&format!("{:?}", WORDS.as_bytes())),
        "nor their bytes"
    );
    assert_eq!(f.data.try_recv(), Ok(DataToBrain::Credit { bytes: 9 }));
    assert_eq!(f.retry(), Ok(()));
    f.assert_plain(2, None);
    assert_eq!(f.retry(), Err(Refused::Busy));
    assert_eq!(f.data.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn changed_display_and_local_queue_failure_spend_offer_without_spending_turn() {
    for display in [
        TypedDisplay {
            dpi: 120,
            ..DISPLAY
        },
        DISPLAY,
    ] {
        let mut f = Fixture::new(Some(MAIN));
        f.arm(1);
        if display == DISPLAY {
            assert!(
                f.link
                    .send_data(&f.session, DataToBrain::Credit { bytes: 9 }),
                "fill local queue"
            );
            assert_eq!(f.send(display), Err(Refused::Offline));
            assert_eq!(f.data.try_recv(), Ok(DataToBrain::Credit { bytes: 9 }));
            assert_eq!(f.exclusion.generation(), 1, "acquired then rolled back");
        } else {
            assert_eq!(f.send(display), Err(Refused::Busy));
            assert_eq!(f.exclusion.generation(), 0, "never touch the new display");
        }
        assert_eq!(f.surfaces.affinities(), PRIOR);
        f.assert_cancel();
        assert_eq!(f.send(DISPLAY), Err(Refused::Busy));
        assert_eq!(f.data.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(f.retry(), Ok(()));
        f.assert_plain(1, Some(MAIN));
    }
}

#[test]
fn exact_held_proof_allows_one_new_promotion_and_named_screen_refusal_recovers_words() {
    let mut f = Fixture::new(None);
    f.arm(1);
    assert_eq!(f.send(DISPLAY), Ok(()));
    let proof = proof(f.queued_picture());
    f.surfaces.controls = Some(f.control.clone());
    f.surfaces.proof_before_restore = true;
    assert!(
        !f.held(2, proof, f.at),
        "another pairing cannot prove coverage"
    );
    assert_eq!(f.surfaces.affinities(), [EXCLUDED; 8]);
    assert!(f.held(1, proof, f.at), "the current lease proves coverage");
    assert_eq!(
        f.control.try_recv(),
        Ok(ControlToBrain::ConfirmTypedScreen(proof))
    );
    assert_eq!(f.surfaces.affinities(), PRIOR);
    f.surfaces.proof_before_restore = false;
    assert!(!f.held(1, proof, f.at), "proof is single use");
    let promoted = ThreadId(9);
    f.runs.apply_state(
        RunId(1),
        promoted,
        Phase::Thinking,
        Some(RunOrigin::Typed { turn: 1 }),
    );
    assert_eq!(f.runs.thread(), Some(promoted));
    f.runs.apply_run_state(
        RunId(1),
        promoted,
        Phase::Failed,
        Some(ErrorKey::ScreenUnavailable.into()),
        None,
    );
    assert_eq!(f.retry(), Ok(()));
    f.assert_plain(2, Some(promoted));
    f.runs.apply_state(
        RunId(2),
        ThreadId(10),
        Phase::Thinking,
        Some(RunOrigin::Typed { turn: 1 }),
    );
    assert_eq!(
        f.runs.thread(),
        Some(promoted),
        "replay cannot promote another New"
    );
}

#[test]
fn esc_retires_the_exact_pre_run_picture_even_when_cancel_queue_is_full() {
    let mut f = Fixture::new(None);
    f.arm(1);
    assert_eq!(f.send(DISPLAY), Ok(()));
    let picture = f.queued_picture();
    let capacity = f.control.capacity().expect("bounded control queue");
    for _ in 0..capacity {
        assert!(
            f.link.send_control(&f.session, ControlToBrain::Pong),
            "fill control queue"
        );
    }
    assert_eq!(f.runs.stop(), None, "no run has been named");
    f.flush();
    assert!(!f.session.open(), "failed cancel invalidates the pair");
    assert_eq!(f.surfaces.affinities(), PRIOR);
    assert!(
        !f.held(1, proof(picture), f.at),
        "the cancelled proof cannot return"
    );
    assert_eq!(
        f.retry(),
        Err(Refused::Busy),
        "the pending New cannot be resubmitted"
    );
}

#[test]
fn held_after_early_restore_or_deadline_cancels_and_does_not_touch_a_new_lease() {
    for early_restore in [false, true] {
        let mut f = Fixture::new(None);
        f.arm(1);
        assert_eq!(f.send(DISPLAY), Ok(()));
        let proof = proof(f.queued_picture());
        if early_restore {
            f.exclusion.cleanup(&mut f.surfaces).expect("simulate loss");
            f.exclusion
                .acquire(1, CaptureFor::Turn(99), f.at, &mut f.surfaces)
                .expect("new owner");
        }
        let now = if early_restore {
            f.at
        } else {
            f.at + Duration::from_secs(3)
        };
        assert!(!f.held(1, proof, now), "no continuous proof");
        f.assert_cancel();
        assert_eq!(
            f.surfaces.affinities(),
            if early_restore { [EXCLUDED; 8] } else { PRIOR }
        );
        assert!(!f.held(1, proof, f.at), "later valid time cannot revive it");
        f.runs.apply_state(
            RunId(1),
            ThreadId(9),
            Phase::Thinking,
            Some(RunOrigin::Typed { turn: 1 }),
        );
        assert_eq!(f.runs.thread(), None, "cancellation prevents promotion");
    }
}
