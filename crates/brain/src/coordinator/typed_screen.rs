//! Exact authority and write settlement for a one-shot typed picture.

use kumochi_protocol::message::{
    ConnectionKind, Content, ControlToShell, ErrorKey, ModelChoice, RunId, RunOrigin, ThreadId,
    TypedScreenOffer,
};

use super::{Availability, BuildKey, Coordinator, Effect, Input, Run, Stale};
use crate::context::{Jpeg, PictureRecipient, Route, RunSnapshot};
use crate::recall::control::Control;
use crate::store::Failure;

/// One eligible existing chat's cancellable lookup of its committed model inheritance.
#[derive(Clone, Debug)]
pub struct ReadTarget {
    pub(crate) control: Control,
    pub(crate) epoch: u64,
    pub(crate) offer: TypedScreenOffer,
    pub(crate) thread: ThreadId,
}

impl PartialEq for ReadTarget {
    fn eq(&self, other: &Self) -> bool {
        (self.epoch, self.offer, self.thread) == (other.epoch, other.offer, other.thread)
    }
}

impl ReadTarget {
    pub(crate) fn reply(&self, inherited: bool) -> Input {
        Input::TypedTargetRead {
            epoch: self.epoch,
            offer: self.offer,
            inherited,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct TypedKey {
    pub(crate) epoch: u64,
    pub(crate) turn: u32,
    pub(crate) offer: TypedScreenOffer,
}

pub(super) struct TypedRun {
    key: TypedKey,
    pub(super) recipient: PictureRecipient,
    pub(super) jpeg: Option<Jpeg>,
    pub(super) attempt: Option<BuildKey>,
    pub(super) flushed: bool,
}

impl std::fmt::Debug for TypedRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypedRun")
            .field("turn", &self.key.turn)
            .field("captured", &self.jpeg.is_some())
            .field("attempt", &self.attempt)
            .field("flushed", &self.flushed)
            .finish_non_exhaustive()
    }
}

impl Coordinator {
    pub(crate) fn typed_recipient(&self, choice: &ModelChoice) -> Option<PictureRecipient> {
        if choice.connection != ConnectionKind::ChatGpt {
            return None;
        }
        let Availability::Ready(ready) = self.availability_of(choice.connection) else {
            return None;
        };
        let route = Route {
            kind: choice.connection,
            tag: ready.connection,
        };
        (ready.models.iter().any(|model| model.slug == choice.model)
            && self.pictured(route, Some(&choice.model)))
        .then(|| PictureRecipient {
            route,
            model: choice.model.clone(),
        })
    }

    pub(crate) fn typed_target(&self, thread: Option<ThreadId>) -> bool {
        thread.is_none_or(|thread| thread == self.main_thread && !self.deleting_thread(thread))
    }

    pub(crate) fn next_typed_turn(&self) -> Option<u32> {
        self.turns_read.checked_add(1)
    }

    pub(crate) fn typed_attempt(
        &mut self,
        thread: Option<ThreadId>,
        effects: &mut Vec<Effect>,
    ) -> bool {
        self.count(effects) && self.admits(thread)
    }

    pub(crate) fn start_typed_picture(
        &mut self,
        key: TypedKey,
        thread: Option<ThreadId>,
        text: Content,
        mut snapshot: RunSnapshot,
        recipient: PictureRecipient,
        jpeg: Jpeg,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        if !self.typed_target(thread)
            || snapshot
                .choice
                .as_ref()
                .and_then(|choice| self.typed_recipient(choice))
                .as_ref()
                != Some(&recipient)
        {
            effects.push(Effect::Control(ControlToShell::TurnRefused {
                turn: key.turn,
                thread,
            }));
            return effects;
        }
        let run = self.start_turn(
            RunOrigin::Typed { turn: key.turn },
            thread,
            text,
            None,
            None,
            &mut effects,
        );
        let main = self.main_thread;
        let current = self.current(run).expect("the typed turn just started");
        snapshot.origin = super::admission::origin(current.thread, main);
        snapshot.connection = recipient.route.tag;
        current.snapshot = Some(snapshot);
        current.pictured = true;
        current.finding = None;
        current.typed_screen = Some(TypedRun {
            key,
            recipient,
            jpeg: Some(jpeg),
            attempt: None,
            flushed: false,
        });
        effects
    }

    pub(crate) fn cancel_typed_picture(
        &mut self,
        key: Option<TypedKey>,
        thread: Option<ThreadId>,
    ) -> Vec<Effect> {
        let run = self
            .current
            .as_ref()
            .filter(|run| {
                thread.is_none_or(|thread| thread == run.thread)
                    && run
                        .typed_screen
                        .as_ref()
                        .is_some_and(|typed| key.is_none_or(|key| key == typed.key))
            })
            .map(|run| run.id);
        let mut effects = Vec::new();
        if let Some(run) = run {
            self.close_grants(run);
            self.fail_typed_picture(run, &mut effects)
                .expect("the typed run is current");
        }
        effects
    }

    pub(super) fn fail_typed_picture(
        &mut self,
        run: RunId,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        self.fail_with(
            run,
            Failure::Engine,
            ErrorKey::ScreenUnavailable.into(),
            effects,
        )
    }

    pub(super) fn typed_picture_current(&self, run: &Run) -> bool {
        let Some(typed) = run.typed_screen.as_ref() else {
            return false;
        };
        run.route == Some(typed.recipient.route)
            && run
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.choice.as_ref())
                .and_then(|choice| self.typed_recipient(choice))
                .as_ref()
                == Some(&typed.recipient)
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use kumochi_protocol::message::{AssistantName, DataToShell};

    use super::*;
    use crate::context::{ConnectionTag, Origin};
    use crate::coordinator::tests::routes::{built_for, choice, jpeg, pictured};
    use crate::coordinator::{LeaseId, SendKey, StoreOp};

    const CHAT: ConnectionKind = ConnectionKind::ChatGpt;
    const MAIN: ThreadId = ThreadId(1);
    const RUN: RunId = RunId(1);
    const FIRST: BuildKey = BuildKey {
        run: RUN,
        round: 1,
        attempt: 1,
    };

    fn pictured_run() -> (Coordinator, ModelChoice, Vec<Effect>) {
        let mut core = Coordinator::new(MAIN, MAIN, RunId(0), Vec::new());
        let choice = choice(CHAT, "m");
        core.handle(Input::Engines(CHAT, pictured(7, &["m"])))
            .unwrap();
        let recipient = core.typed_recipient(&choice).unwrap();
        let key = TypedKey {
            epoch: 11,
            turn: 1,
            offer: TypedScreenOffer(NonZeroU64::MIN),
        };
        let snapshot = RunSnapshot {
            origin: Origin::Kumo,
            assistant: AssistantName::new("Kumo").unwrap(),
            user: None,
            choice: Some(choice.clone()),
            connection: ConnectionTag(0),
        };
        let mut effects = Vec::new();
        assert!(core.typed_attempt(None, &mut effects));
        let effects = core.start_typed_picture(
            key,
            None,
            "private words".into(),
            snapshot,
            recipient,
            jpeg(),
        );
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Store(StoreOp::BeginTurn {
                thread: ThreadId(2),
                run: RUN,
                screen: None,
                ..
            })
        )));
        assert!(!format!("{effects:?}").contains("private words"));
        core.handle(Input::TurnStored { run: RUN }).unwrap();
        let effects = core.handle(built_for(FIRST, &choice)).unwrap();
        (core, choice, effects)
    }

    fn grant(core: &mut Coordinator) -> LeaseId {
        let effects = core
            .handle(Input::SendReady {
                key: SendKey::Build(FIRST),
                hold: None,
            })
            .unwrap();
        effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Grant(lease) => Some(*lease),
                _ => None,
            })
            .unwrap()
    }

    fn serialized(effects: &[Effect], images: usize) {
        let request = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::StartEngine { request } => Some(request),
                _ => None,
            })
            .unwrap();
        let mut body = crate::engines::responses::body(&request.request).unwrap();
        assert_eq!(body["model"], "m");
        assert_eq!(body["store"], false);
        assert_eq!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|value| value["content"][0]["type"] == "input_image")
                .count(),
            images
        );
        crate::engines::responses::wipe_images(&mut body);
    }

    #[test]
    fn typed_picture_serializes_once_and_only_its_full_flush_badges_the_turn() {
        let (mut core, choice, effects) = pictured_run();
        serialized(&effects, 1);
        let lease = grant(&mut core);
        let wrong = LeaseId {
            send: lease.send + 1,
            ..lease
        };
        assert_eq!(core.handle(Input::SendFlushed { lease: wrong }), Err(Stale));
        let effects = core.handle(Input::SendFlushed { lease }).unwrap();
        assert!(effects.contains(&Effect::Store(StoreOp::FlushedTypedScreen { run: RUN })));
        assert!(effects.contains(&Effect::Data(DataToShell::EventsChanged {
            thread: ThreadId(2)
        })));
        assert_eq!(core.handle(Input::SendFlushed { lease }), Err(Stale));
        core.build(RUN, 2, &mut Vec::new());
        let second = BuildKey {
            run: RUN,
            round: 2,
            attempt: 2,
        };
        let effects = core.handle(built_for(second, &choice)).unwrap();
        serialized(&effects, 0);
        let effects = core
            .handle(Input::SendReady {
                key: SendKey::Build(second),
                hold: None,
            })
            .unwrap();
        assert!(effects.iter().any(|effect| matches!(effect, Effect::Grant(lease) if lease.key == SendKey::Build(second) && !core.typed_picture_lease(*lease))));
    }

    #[test]
    fn typed_picture_partial_settlement_does_not_badge_or_allow_an_image_retry() {
        let (mut core, _, _) = pictured_run();
        let lease = grant(&mut core);
        let effects = core.handle(Input::SendSettled { lease }).unwrap();
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Store(StoreOp::FlushedTypedScreen { .. })))
        );
        let effects = core
            .handle(Input::SendReady {
                key: SendKey::Build(FIRST),
                hold: None,
            })
            .unwrap();
        assert!(effects.contains(&Effect::Rebuild(SendKey::Build(FIRST))));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Grant(_)))
        );
    }

    #[test]
    fn typed_picture_closes_write_admission_on_capability_loss_or_same_slug_replacement() {
        for (tag, models) in [(7, &[][..]), (8, &["m"][..])] {
            let (mut core, _, _) = pictured_run();
            let lease = grant(&mut core);
            let grant = core.picture_grant(lease).unwrap();
            assert!(grant.is_open());
            core.handle(Input::Engines(CHAT, pictured(tag, models)))
                .unwrap();
            assert!(grant.reserve_write().is_none());
            core.handle(Input::Engines(CHAT, pictured(7, &["m"])))
                .unwrap();
            assert!(grant.reserve_write().is_none());
        }
    }
}
