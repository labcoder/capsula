//! Typed composer recovery routes and connection-scoped renewal notice coalescing.

use std::{fmt, rc::Rc};

use kumochi_protocol::message::{AccountOp, Action, ErrorKey, RunId, ThreadId, UserError};

use crate::settings::Field;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Run(RunId),
    Account,
    Settings(Option<Field>),
    Drawing { thread: ThreadId, run: RunId },
}

#[derive(Clone, PartialEq, Eq)]
pub struct Notice {
    pub error: UserError,
    pub source: Source,
}

impl fmt::Debug for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notice")
            .field("key", &self.error.key())
            .field("args", &self.error.args().len())
            .field("actions", &self.error.actions().len())
            .field(
                "source",
                &match self.source {
                    Source::Run(_) => "run",
                    Source::Account => "account",
                    Source::Settings(_) => "settings",
                    Source::Drawing { .. } => "drawing",
                },
            )
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    RunRetry(RunId),
    Account(AccountOp),
    SettingsRetry(Field),
    Accounts,
    Settings,
    Usage,
}

impl Notice {
    pub fn route(&self, action: Action) -> Option<Route> {
        Some(match action {
            Action::Retry => match self.source {
                Source::Run(run) => Route::RunRetry(run),
                Source::Account => Route::Account(AccountOp::Retry),
                Source::Settings(Some(field)) => Route::SettingsRetry(field),
                Source::Settings(None) | Source::Drawing { .. } => return None,
            },
            Action::SignIn => match self.error.key() {
                ErrorKey::ClaudeCodeSignedOut | ErrorKey::AnthropicKeyRejected => Route::Accounts,
                _ => Route::Account(AccountOp::SignIn),
            },
            Action::Enable => Route::Account(AccountOp::EnablePlan),
            Action::ManageUsage => Route::Usage,
            Action::OpenSettings => match self.source {
                Source::Settings(_) => Route::Settings,
                Source::Run(_) | Source::Account | Source::Drawing { .. } => Route::Accounts,
            },
        })
    }
}

#[derive(Default, Debug)]
enum Renewal {
    #[default]
    Fresh,
    AccountShown,
    RunOwned,
    Dismissed,
}

#[derive(Default)]
pub struct ChatNotice {
    shown: Option<Rc<Notice>>,
    renewal: Renewal,
}

impl fmt::Debug for ChatNotice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatNotice")
            .field("shown", &self.shown)
            .field("renewal", &self.renewal)
            .finish()
    }
}

impl ChatNotice {
    pub fn for_thread(&self, thread: Option<ThreadId>) -> Option<&Rc<Notice>> {
        self.shown.as_ref().filter(|notice| {
            !matches!(notice.source, Source::Drawing { thread: owner, .. } if Some(owner) != thread)
        })
    }

    pub fn shown(&self) -> Option<&Notice> {
        self.shown.as_deref()
    }

    pub fn present(&mut self, error: UserError, source: Source) {
        if error.key() == ErrorKey::RenewalUnconfirmed {
            let promoting = matches!(
                (&self.renewal, source),
                (Renewal::AccountShown, Source::Run(_))
            );
            self.renewal = match (&self.renewal, source) {
                (Renewal::Fresh | Renewal::AccountShown, Source::Run(_)) => Renewal::RunOwned,
                (Renewal::Fresh, _) => Renewal::AccountShown,
                _ => return,
            };
            if promoting
                && self
                    .shown
                    .as_ref()
                    .is_none_or(|notice| notice.error.key() != ErrorKey::RenewalUnconfirmed)
            {
                return;
            }
        }
        let notice = Notice { error, source };
        if self.shown() != Some(&notice) {
            self.shown = Some(Rc::new(notice));
        }
    }

    pub fn dismiss(&mut self) {
        if self
            .shown
            .as_ref()
            .is_some_and(|notice| notice.error.key() == ErrorKey::RenewalUnconfirmed)
        {
            self.renewal = Renewal::Dismissed;
        }
        self.shown = None;
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn recovered(&mut self) {
        self.renewal = Renewal::Fresh;
        if self
            .shown
            .as_ref()
            .is_some_and(|notice| notice.error.key() == ErrorKey::RenewalUnconfirmed)
        {
            self.shown = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use kumochi_protocol::message::{AccountOp, Action, ErrorArg, ErrorKey, RunId};

    use super::{ChatNotice, Notice, Route, Source};

    #[test]
    fn a_late_drawing_notice_stays_with_its_original_thread_and_run() {
        use kumochi_protocol::message::ThreadId;
        let mut notices = ChatNotice::default();
        let source = Source::Drawing {
            thread: ThreadId(7),
            run: RunId(3),
        };
        notices.present(ErrorKey::DrawingReplacementFailed.into(), source);
        assert_eq!(notices.for_thread(Some(ThreadId(8))), None);
        assert_eq!(notices.for_thread(None), None);
        assert_eq!(
            notices.for_thread(Some(ThreadId(7))).map(Rc::as_ref),
            Some(&Notice {
                error: ErrorKey::DrawingReplacementFailed.into(),
                source,
            })
        );
        notices.clear();
        assert_eq!(notices.for_thread(Some(ThreadId(7))), None);
    }
    #[test]
    fn equal_payload_gets_new_identity_only_after_the_notice_changes() {
        let mut notices = ChatNotice::default();
        let present_offline = |notices: &mut ChatNotice| {
            notices.present(ErrorKey::AccountOffline.into(), Source::Account);
            Rc::clone(notices.for_thread(None).expect("offline notice"))
        };
        let original = present_offline(&mut notices);
        assert!(Rc::ptr_eq(&original, &present_offline(&mut notices)));
        notices.present(ErrorKey::TrustStoreUnavailable.into(), Source::Account);
        let restored = present_offline(&mut notices);
        assert_eq!(original, restored);
        assert!(!Rc::ptr_eq(&original, &restored));

        notices.clear();
        let next = present_offline(&mut notices);
        assert_eq!(restored, next);
        assert!(!Rc::ptr_eq(&restored, &next));
    }

    use crate::settings::Field;

    #[test]
    fn renewal_coalesces_in_both_orders_with_run_ownership() {
        for sources in [
            [Source::Account, Source::Run(RunId(7))],
            [Source::Run(RunId(7)), Source::Account],
        ] {
            let mut notices = ChatNotice::default();
            for source in sources {
                notices.present(ErrorKey::RenewalUnconfirmed.into(), source);
            }
            assert_eq!(
                notices.shown().map(|notice| notice.route(Action::Retry)),
                Some(Some(Route::RunRetry(RunId(7))))
            );
        }
    }

    #[test]
    fn dismissal_blocks_the_delayed_second_frame_in_both_orders() {
        for sources in [
            [Source::Account, Source::Run(RunId(7))],
            [Source::Run(RunId(7)), Source::Account],
        ] {
            let mut notices = ChatNotice::default();
            notices.present(ErrorKey::RenewalUnconfirmed.into(), sources[0]);
            notices.dismiss();
            notices.present(ErrorKey::RenewalUnconfirmed.into(), sources[1]);
            assert_eq!(notices.shown(), None);
        }
    }

    #[test]
    fn duplicate_terminals_do_not_replace_the_first_run_or_resurrect_it() {
        let mut notices = ChatNotice::default();
        notices.present(ErrorKey::RenewalUnconfirmed.into(), Source::Run(RunId(7)));
        notices.present(ErrorKey::RenewalUnconfirmed.into(), Source::Run(RunId(8)));
        assert_eq!(
            notices.shown().map(|notice| notice.route(Action::Retry)),
            Some(Some(Route::RunRetry(RunId(7))))
        );
        notices.dismiss();
        notices.present(ErrorKey::RenewalUnconfirmed.into(), Source::Run(RunId(7)));
        assert_eq!(notices.shown(), None);
    }

    #[test]
    fn general_only_needs_no_run_and_ignores_duplicate_payloads() {
        let mut notices = ChatNotice::default();
        notices.present(ErrorKey::RenewalUnconfirmed.into(), Source::Account);
        let duplicate = kumochi_protocol::message::UserError::new(
            ErrorKey::RenewalUnconfirmed,
            vec![ErrorArg::Count(2)],
            vec![Action::SignIn],
        )
        .expect("bounded duplicate");
        notices.present(duplicate, Source::Account);
        assert_eq!(
            notices.shown(),
            Some(&Notice {
                error: ErrorKey::RenewalUnconfirmed.into(),
                source: Source::Account,
            })
        );
    }

    #[test]
    fn clear_and_recovery_start_new_incidents() {
        let mut notices = ChatNotice::default();
        for reset in [ChatNotice::clear, ChatNotice::recovered] {
            notices.present(ErrorKey::RenewalUnconfirmed.into(), Source::Account);
            notices.dismiss();
            reset(&mut notices);
            notices.present(ErrorKey::RenewalUnconfirmed.into(), Source::Run(RunId(9)));
            assert_eq!(
                notices.shown().map(|notice| notice.route(Action::Retry)),
                Some(Some(Route::RunRetry(RunId(9))))
            );
            reset(&mut notices);
            assert_eq!(notices.shown(), None);
        }
    }

    #[test]
    fn late_run_renewal_does_not_replace_or_restore_a_displaced_notice() {
        for dismiss_newer in [false, true] {
            let mut notices = ChatNotice::default();
            let unrelated = Notice {
                error: ErrorKey::SettingsRefused.into(),
                source: Source::Settings(None),
            };
            notices.present(ErrorKey::RenewalUnconfirmed.into(), Source::Account);
            notices.present(unrelated.error.clone(), unrelated.source);
            if dismiss_newer {
                notices.dismiss();
            }
            let expected = (!dismiss_newer).then_some(&unrelated);
            notices.present(ErrorKey::RenewalUnconfirmed.into(), Source::Run(RunId(7)));
            assert_eq!(notices.shown(), expected);
            assert!(matches!(notices.renewal, super::Renewal::RunOwned));
            notices.present(ErrorKey::RenewalUnconfirmed.into(), Source::Run(RunId(8)));
            notices.present(ErrorKey::RenewalUnconfirmed.into(), Source::Account);
            assert_eq!(notices.shown(), expected);
        }
    }

    #[test]
    fn unrelated_errors_survive_recovery_and_do_not_reset_dismissal() {
        let mut notices = ChatNotice::default();
        notices.present(ErrorKey::RenewalUnconfirmed.into(), Source::Account);
        notices.dismiss();
        notices.present(ErrorKey::SettingsRefused.into(), Source::Settings(None));
        notices.present(ErrorKey::RenewalUnconfirmed.into(), Source::Run(RunId(7)));
        let unrelated = Notice {
            error: ErrorKey::SettingsRefused.into(),
            source: Source::Settings(None),
        };
        assert_eq!(notices.shown(), Some(&unrelated));
        notices.recovered();
        assert_eq!(notices.shown(), Some(&unrelated));
        notices.dismiss();
        notices.present(ErrorKey::AccountOffline.into(), Source::Account);
        assert_eq!(
            notices.shown().map(|notice| notice.error.key()),
            Some(ErrorKey::AccountOffline)
        );
    }

    #[test]
    fn actions_route_by_origin_and_provider() {
        let mut notice = Notice {
            error: ErrorKey::SessionEnded.into(),
            source: Source::Run(RunId(7)),
        };
        assert_eq!(notice.route(Action::Retry), Some(Route::RunRetry(RunId(7))));
        assert_eq!(
            notice.route(Action::SignIn),
            Some(Route::Account(AccountOp::SignIn))
        );
        assert_eq!(
            notice.route(Action::Enable),
            Some(Route::Account(AccountOp::EnablePlan))
        );
        assert_eq!(notice.route(Action::ManageUsage), Some(Route::Usage));
        assert_eq!(notice.route(Action::OpenSettings), Some(Route::Accounts));
        notice.source = Source::Account;
        assert_eq!(
            notice.route(Action::Retry),
            Some(Route::Account(AccountOp::Retry))
        );
        assert_eq!(notice.route(Action::OpenSettings), Some(Route::Accounts));
        notice.source = Source::Settings(Some(Field::Assistant));
        assert_eq!(
            notice.route(Action::Retry),
            Some(Route::SettingsRetry(Field::Assistant))
        );
        assert_eq!(notice.route(Action::OpenSettings), Some(Route::Settings));
        notice.source = Source::Settings(None);
        assert_eq!(notice.route(Action::Retry), None);
        for key in [
            ErrorKey::ClaudeCodeSignedOut,
            ErrorKey::AnthropicKeyRejected,
        ] {
            notice.error = key.into();
            assert_eq!(notice.route(Action::SignIn), Some(Route::Accounts));
        }
    }

    #[test]
    fn outer_debug_redacts_argument_and_run_sentinels() {
        let mut notices = ChatNotice::default();
        notices.present(
            kumochi_protocol::message::UserError::new(
                ErrorKey::RenewalUnconfirmed,
                vec![ErrorArg::Count(314_159_265)],
                vec![Action::Retry],
            )
            .expect("bounded sentinel"),
            Source::Run(RunId(271_828_182)),
        );
        let debug = format!("{notices:?}");
        for sentinel in ["314159265", "271828182"] {
            assert!(!debug.contains(sentinel), "argument or run leaked");
            assert!(
                !debug.contains(&format!("{:?}", sentinel.as_bytes())),
                "sentinel bytes leaked"
            );
        }
    }
}
