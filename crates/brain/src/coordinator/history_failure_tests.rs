use kumochi_protocol::message::{DeliveryProof, ErrorKey, HistoryFailure, HistoryFailureCause};

use super::{
    Coordinator, Effect, Failure, Input, R, RunEnd, SendKey, TurnEvent, end_run, engine_key,
    started,
};
use crate::connections::model::error::{Failure as Responses, ProviderError, RejectionOrigin};
use crate::engines::{ClaudeSignOutStage, EngineFailure, Finish};

const INVALID_USER: &[u8] =
    include_bytes!("../../fixtures/responses/subscription_sharing_invalid_user.json");
const USAGE_LIMIT: &[u8] =
    include_bytes!("../../fixtures/responses/subscription_sharing_usage_limit_exceeded.json");

fn grant(core: &mut Coordinator) {
    let key = SendKey::Build(engine_key(core, R));
    let effects = core.handle(Input::SendReady { key, hold: None }).unwrap();
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Grant(lease) if lease.key == key))
    );
}

fn finish(core: &mut Coordinator, failure: EngineFailure) -> Vec<Effect> {
    core.handle(Input::Engine {
        key: engine_key(core, R),
        event: TurnEvent::Finished(Finish::Failed(failure)),
    })
    .unwrap()
}

fn saved(cause: HistoryFailureCause, delivery: DeliveryProof) -> RunEnd {
    RunEnd::Failed(Failure::Connection(HistoryFailure { cause, delivery }))
}

#[test]
fn first_responses_http_rejections_keep_the_wire_cause_and_are_not_not_sent() {
    for (status, body, cause) in [
        (401, INVALID_USER, HistoryFailureCause::ChatGptSignIn),
        (429, USAGE_LIMIT, HistoryFailureCause::ChatGptUsageLimit),
    ] {
        let mut core = started();
        grant(&mut core);
        let refusal = Responses::Provider(ProviderError {
            origin: RejectionOrigin::AfterUpload,
            ..ProviderError::answered(status, body)
        });
        let effects = finish(&mut core, EngineFailure::Stream(refusal));
        assert!(effects.contains(&end_run(
            None,
            None,
            saved(cause, DeliveryProof::NotAccepted)
        )));
    }
}

#[test]
fn a_provider_error_after_answer_text_keeps_the_partial_answer() {
    let mut core = started();
    grant(&mut core);
    core.handle(Input::Engine {
        key: engine_key(&core, R),
        event: TurnEvent::TextDelta("a partial answer".into()),
    })
    .unwrap();
    let refusal = Responses::Provider(ProviderError::answered(200, USAGE_LIMIT));
    let effects = finish(&mut core, EngineFailure::Stream(refusal));
    assert!(effects.contains(&end_run(
        Some("a partial answer"),
        None,
        saved(
            HistoryFailureCause::ChatGptUsageLimit,
            DeliveryProof::PartialAnswer
        )
    )));
}

#[test]
fn ambiguous_delivery_after_a_grant_never_becomes_not_sent() {
    let session_ended = EngineFailure::Access(Some(ErrorKey::SessionEnded.into()));
    let claude_sign_out = |stage| EngineFailure::ClaudeSignedOut {
        stage,
        notice: ErrorKey::ClaudeCodeSignedOut.into(),
    };
    for refusal in [
        EngineFailure::Stream(Responses::Transport),
        EngineFailure::Stream(Responses::Provider(ProviderError::answered(
            401,
            INVALID_USER,
        ))),
        EngineFailure::Stream(Responses::Provider(ProviderError::answered(
            200,
            USAGE_LIMIT,
        ))),
        claude_sign_out(ClaudeSignOutStage::AfterSeed),
        session_ended.clone(),
    ] {
        let mut core = started();
        grant(&mut core);
        let effects = finish(&mut core, refusal);
        assert!(effects.contains(&end_run(None, None, RunEnd::Failed(Failure::Engine))));
    }

    let mut core = started();
    let effects = finish(&mut core, session_ended);
    assert!(effects.contains(&end_run(
        None,
        None,
        saved(HistoryFailureCause::ChatGptSignIn, DeliveryProof::NotSent)
    )));
}
