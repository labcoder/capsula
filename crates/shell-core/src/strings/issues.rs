use kumochi_protocol::message::{
    ActionState, FailReason, IssueCard, IssueRefused, IssueUnavailableKind, LookupResult,
};

use super::{FluentArgs, Strings};

impl Strings {
    pub fn issue_feedback(&self, feedback: crate::threads::IssueFeedback) -> String {
        let reason = match feedback {
            crate::threads::IssueFeedback::QueueFailed => {
                return self.get(super::Text::IssueQueueFailed);
            }
            crate::threads::IssueFeedback::Unavailable(kind) => {
                return self.get(match kind {
                    IssueUnavailableKind::Action => super::Text::IssueActionUnavailable,
                    IssueUnavailableKind::ReviewNotSaved => super::Text::IssueReviewNotSaved,
                    IssueUnavailableKind::ReviewCapacity => super::Text::IssueReviewCapacity,
                    IssueUnavailableKind::AttemptNotSaved => super::Text::IssueAttemptNotSaved,
                });
            }
            crate::threads::IssueFeedback::Refused(reason) => reason,
        };
        let mut args = FluentArgs::new();
        args.set(
            "reason",
            match reason {
                IssueRefused::StaleApproval => "approval",
                IssueRefused::ContentChanged => "content",
                IssueRefused::NotConnected => "connection",
                IssueRefused::OtherAccount => "account",
                IssueRefused::RepoNotListed => "repository",
                IssueRefused::NotAllowedNow => "busy",
                IssueRefused::ThreadGone => "thread",
                IssueRefused::UnresolvedDraft { .. } => "unresolved",
            },
        );
        self.format("issue-refusal", Some(&args))
    }

    pub fn issue_status(&self, state: &ActionState) -> String {
        let mut args = FluentArgs::new();
        let id = match state {
            ActionState::Draft => "issue-status-draft",
            ActionState::Sending => "issue-status-sending",
            ActionState::Filed {
                number,
                url,
                confirmed_by_user,
                ..
            } => {
                args.set("number", number.to_string());
                args.set("url", url.as_str());
                args.set(
                    "confirmation",
                    if *confirmed_by_user { "user" } else { "remote" },
                );
                "issue-status-filed"
            }
            ActionState::Failed(reason) => {
                args.set(
                    "reason",
                    match reason {
                        FailReason::NotConnected => "connection",
                        FailReason::PermissionDenied => "permission",
                        FailReason::NotFound => "repository",
                        FailReason::InvalidIssue => "content",
                        FailReason::RateLimited => "rate",
                        FailReason::Network => "network",
                        FailReason::PreflightFailed => "preflight",
                        FailReason::Interrupted => "interrupted",
                        FailReason::Rejected => "rejected",
                    },
                );
                "issue-status-failed"
            }
            ActionState::Unknown { lookup, .. } => {
                args.set("number", "");
                args.set("url", "");
                let lookup = match lookup {
                    LookupResult::Pending => "pending",
                    LookupResult::None => "none",
                    LookupResult::Several => "several",
                    LookupResult::Failed => "failed",
                    LookupResult::Unplaced { number, url } => {
                        args.set("number", number.to_string());
                        args.set("url", url.as_str());
                        "unplaced"
                    }
                };
                args.set("lookup", lookup);
                "issue-status-unknown"
            }
            ActionState::Discarded { maybe_filed } => {
                args.set("risk", if *maybe_filed { "possible" } else { "none" });
                "issue-status-discarded"
            }
        };
        self.format(id, Some(&args))
    }

    pub fn issue_details(&self, card: &IssueCard) -> String {
        let mut args = FluentArgs::new();
        let repo = card.repo.as_ref().map_or_else(
            || self.format("issue-repository-unbound", None),
            |repo| repo.as_str().to_owned(),
        );
        let account = card.account.as_ref().map_or_else(
            || self.format("issue-account-unbound", None),
            |account| account.as_str().to_owned(),
        );
        args.set("repo", repo);
        args.set("account", account);
        args.set("body", card.body.as_str());
        self.format("issue-details", Some(&args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumochi_protocol::message::IssueUrl;

    #[test]
    fn review_failures_do_not_confuse_preconfirmation_with_possible_post_handoff() {
        let strings = Strings::english();
        for (kind, text) in [
            (
                IssueUnavailableKind::ReviewNotSaved,
                "The review couldn't be saved. Nothing was sent to GitHub. Restore storage, then explicitly review again.",
            ),
            (
                IssueUnavailableKind::ReviewCapacity,
                "Other reviews are open or finishing. Finish or cancel one, then review again.",
            ),
            (
                IssueUnavailableKind::AttemptNotSaved,
                "The confirmed outcome couldn't be saved. GitHub may have received this issue. Do not file it again.",
            ),
        ] {
            assert_eq!(
                strings.issue_feedback(crate::threads::IssueFeedback::Unavailable(kind)),
                text
            );
        }
        let recovered = strings.issue_feedback(crate::threads::IssueFeedback::Unavailable(
            IssueUnavailableKind::Action,
        ));
        assert!(recovered.contains("Review the saved details"));
        let unsent = strings.issue_feedback(crate::threads::IssueFeedback::QueueFailed);
        assert!(unsent.contains("wasn't queued"));
        assert_eq!(
            strings.get(super::super::Text::IssueReviewPending),
            "Saving a fresh review…"
        );
        assert_eq!(
            strings.get(super::super::Text::IssueConsentOpen),
            "Review to file…"
        );
    }

    #[test]
    fn saved_status_copy_keeps_uncertain_candidates_and_confirmations_distinct() {
        let strings = Strings::english();
        assert_eq!(strings.issue_status(&ActionState::Draft), "Draft");
        assert_eq!(
            strings.issue_status(&ActionState::Sending),
            "Preparing or sending"
        );
        let url = IssueUrl::new("https://github.com/o/r/issues/18446744073709551615").unwrap();
        for (confirmed_by_user, line) in
            [(false, "Confirmed by GitHub."), (true, "Confirmed by you.")]
        {
            assert_eq!(
                strings.issue_status(&ActionState::Filed {
                    number: u64::MAX,
                    url: url.clone(),
                    confirmed_by_user,
                    maybe_duplicate: true,
                }),
                format!("Filed #18446744073709551615\n{line}\n{}", url.as_str())
            );
        }
        for (lookup, line) in [
            (
                LookupResult::Pending,
                "No completed lookup is available.".to_owned(),
            ),
            (
                LookupResult::None,
                "No matching issue was visible; it may still exist.".to_owned(),
            ),
            (
                LookupResult::Several,
                "More than one issue matched.".to_owned(),
            ),
            (
                LookupResult::Failed,
                "The lookup failed or was incomplete.".to_owned(),
            ),
            (
                LookupResult::Unplaced {
                    number: u64::MAX,
                    url: url.clone(),
                },
                format!(
                    "Candidate #18446744073709551615 is not confirmed as this issue.\n{}",
                    url.as_str()
                ),
            ),
        ] {
            assert_eq!(
                strings.issue_status(&ActionState::Unknown {
                    lookup,
                    maybe_duplicate: false
                }),
                format!("Outcome unknown. An issue may already exist on GitHub.\n{line}")
            );
        }
        assert_eq!(
            strings.issue_status(&ActionState::Discarded { maybe_filed: false }),
            "Dismissed"
        );
        assert_eq!(
            strings.issue_status(&ActionState::Discarded { maybe_filed: true }),
            "Dismissed. An issue may already exist on GitHub."
        );
    }

    #[test]
    fn unbound_saved_details_preserve_body_text_without_interpreting_it() {
        use kumochi_protocol::message::{
            ContentHash, DraftId, IssueBody, IssueTitle, StateVersion, ThreadId,
        };
        let card = IssueCard {
            draft: DraftId(1),
            thread: ThreadId(2),
            repo: None,
            account: None,
            title: IssueTitle::new("title").unwrap(),
            body: IssueBody::new("Literal { $repo } — 窓\nsecond line").unwrap(),
            hash: ContentHash([1; 32]),
            approval: None,
            version: StateVersion(1),
            state: ActionState::Draft,
        };
        assert_eq!(
            Strings::english().issue_details(&card),
            "Repository: Not selected\nBound account: Not bound\n\nLiteral { $repo } — 窓\nsecond line"
        );
    }

    #[test]
    fn definite_failure_copy_does_not_claim_that_nothing_was_sent() {
        let strings = Strings::english();
        for (reason, line) in [
            (FailReason::NotConnected, "GitHub wasn't connected."),
            (
                FailReason::PermissionDenied,
                "The account didn't have permission.",
            ),
            (FailReason::NotFound, "The repository wasn't found."),
            (
                FailReason::InvalidIssue,
                "GitHub refused the issue's content.",
            ),
            (
                FailReason::RateLimited,
                "GitHub required a wait before another attempt.",
            ),
            (FailReason::Network, "The connection failed before sending."),
            (
                FailReason::PreflightFailed,
                "The check before sending couldn't complete safely.",
            ),
            (
                FailReason::Interrupted,
                "The attempt was interrupted before sending.",
            ),
            (FailReason::Rejected, "GitHub refused the attempt."),
        ] {
            assert_eq!(
                strings.issue_status(&ActionState::Failed(reason)),
                format!("Not filed. No issue was created by this attempt.\n{line}")
            );
        }
    }
}
