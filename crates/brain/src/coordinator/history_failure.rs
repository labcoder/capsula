use kumochi_protocol::message::{DeliveryProof, ErrorKey, HistoryFailure, HistoryFailureCause};

use crate::connections::{
    anthropic::error::{Failure as Anthropic, Kind},
    model::error::{Code, Failure as Responses, RejectionOrigin},
};
use crate::engines::{EngineFailure, Finish};
use crate::store::Failure;

use super::{BuildKey, Run};

pub(super) fn needs_chatgpt_sign_in(key: ErrorKey) -> bool {
    matches!(key, ErrorKey::SessionEnded | ErrorKey::RenewalUnconfirmed)
}

pub(super) fn from_finish(key: BuildKey, run: &Run, finish: &Finish) -> Failure {
    let Finish::Failed(failure) = finish else {
        return Failure::Engine;
    };
    let (cause, first) = match failure {
        EngineFailure::Access(Some(notice)) if needs_chatgpt_sign_in(notice.key()) => (
            HistoryFailureCause::ChatGptSignIn,
            Some(DeliveryProof::NotSent),
        ),
        EngineFailure::Stream(Responses::Provider(error))
            if belongs_to_attempt(error.status, error.origin) =>
        {
            let cause = match (error.code, error.status) {
                (Some(Code::UsageLimit), _) => HistoryFailureCause::ChatGptUsageLimit,
                (Some(Code::InvalidUser), _) | (None | Some(Code::Other), 401) => {
                    HistoryFailureCause::ChatGptSignIn
                }
                _ => return Failure::Engine,
            };
            (cause, http_rejection(error.status))
        }
        EngineFailure::Anthropic(Anthropic::Provider(error))
            if belongs_to_attempt(error.status(), error.origin) =>
        {
            let cause = match error.kind() {
                Kind::Authentication => HistoryFailureCause::AnthropicKeyRejected,
                Kind::Credit => HistoryFailureCause::AnthropicCredit,
                _ => return Failure::Engine,
            };
            (cause, http_rejection(error.status()))
        }
        EngineFailure::ClaudeSignedOut { stage, .. } => (
            HistoryFailureCause::ClaudeCodeSignedOut,
            stage
                .audit_launch()
                .is_some()
                .then_some(DeliveryProof::NotSent),
        ),
        _ => return Failure::Engine,
    };
    let first = match first {
        Some(DeliveryProof::NotSent) if key.round == 1 && run.progress.first_grant.is_none() => {
            first
        }
        Some(DeliveryProof::NotAccepted)
            if key.round == 1 && run.progress.first_grant == Some(key) =>
        {
            first
        }
        _ => None,
    };
    from_evidence(run, cause, first)
}

fn belongs_to_attempt(status: u16, origin: RejectionOrigin) -> bool {
    status == 200 || origin == RejectionOrigin::AfterUpload
}

fn http_rejection(status: u16) -> Option<DeliveryProof> {
    (400..500)
        .contains(&status)
        .then_some(DeliveryProof::NotAccepted)
}

pub(super) fn from_evidence(
    run: &Run,
    cause: HistoryFailureCause,
    first: Option<DeliveryProof>,
) -> Failure {
    let delivery = if run.progress.text_bytes > 0 {
        DeliveryProof::PartialAnswer
    } else if let Some(proof) = first {
        proof
    } else {
        return Failure::Engine;
    };
    Failure::Connection(HistoryFailure { cause, delivery })
}
