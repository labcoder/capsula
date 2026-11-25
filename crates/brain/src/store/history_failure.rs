use kumochi_protocol::message::{DeliveryProof, HistoryFailure, HistoryFailureCause};

pub(super) const MAX_REASON_BYTES: u32 = 48;

pub(super) fn encode(failure: HistoryFailure) -> String {
    let cause = match failure.cause {
        HistoryFailureCause::ChatGptSignIn => "chatgpt_sign_in",
        HistoryFailureCause::ChatGptUsageLimit => "chatgpt_usage_limit",
        HistoryFailureCause::AnthropicKeyRejected => "anthropic_key_rejected",
        HistoryFailureCause::AnthropicCredit => "anthropic_credit",
        HistoryFailureCause::ClaudeCodeSignedOut => "claude_code_signed_out",
    };
    let delivery = match failure.delivery {
        DeliveryProof::NotSent => "not_sent",
        DeliveryProof::NotAccepted => "not_accepted",
        DeliveryProof::PartialAnswer => "partial_answer",
    };
    format!("connection/{cause}/{delivery}")
}

pub(super) fn decode(reason: &[u8]) -> Option<HistoryFailure> {
    let (cause, delivery) = std::str::from_utf8(reason)
        .ok()?
        .strip_prefix("connection/")?
        .split_once('/')?;
    Some(HistoryFailure {
        cause: match cause {
            "chatgpt_sign_in" => HistoryFailureCause::ChatGptSignIn,
            "chatgpt_usage_limit" => HistoryFailureCause::ChatGptUsageLimit,
            "anthropic_key_rejected" => HistoryFailureCause::AnthropicKeyRejected,
            "anthropic_credit" => HistoryFailureCause::AnthropicCredit,
            "claude_code_signed_out" => HistoryFailureCause::ClaudeCodeSignedOut,
            _ => return None,
        },
        delivery: match delivery {
            "not_sent" => DeliveryProof::NotSent,
            "not_accepted" => DeliveryProof::NotAccepted,
            "partial_answer" => DeliveryProof::PartialAnswer,
            _ => return None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_codec_keeps_closed_codes_within_the_read_bound() {
        let causes = [
            (HistoryFailureCause::ChatGptSignIn, "chatgpt_sign_in"),
            (
                HistoryFailureCause::ChatGptUsageLimit,
                "chatgpt_usage_limit",
            ),
            (
                HistoryFailureCause::AnthropicKeyRejected,
                "anthropic_key_rejected",
            ),
            (HistoryFailureCause::AnthropicCredit, "anthropic_credit"),
            (
                HistoryFailureCause::ClaudeCodeSignedOut,
                "claude_code_signed_out",
            ),
        ];
        let mut largest = 0;
        for (cause, code) in causes {
            for (delivery, suffix) in [
                (DeliveryProof::NotSent, "not_sent"),
                (DeliveryProof::NotAccepted, "not_accepted"),
                (DeliveryProof::PartialAnswer, "partial_answer"),
            ] {
                let failure = HistoryFailure { cause, delivery };
                let reason = encode(failure);
                assert_eq!(reason, format!("connection/{code}/{suffix}"));
                largest = largest.max(reason.len());
                assert_eq!(decode(reason.as_bytes()), Some(failure));
            }
        }
        assert_eq!(largest, usize::try_from(MAX_REASON_BYTES).unwrap());
        for reason in [
            b"".as_slice(),
            b"engine",
            b"crash",
            b"connection/chatgpt_sign_in",
            b"connection/chatgpt_sign_in/timeout",
            b"connection/not_a_cause/not_sent",
            b"connection/chatgpt_sign_in/not_sent/extra",
            b"connection/chatgpt_sign_in/not_sent\0",
            b"connection/chatgpt_sign_in/\xff",
        ] {
            assert_eq!(decode(reason), None);
        }
    }
}
