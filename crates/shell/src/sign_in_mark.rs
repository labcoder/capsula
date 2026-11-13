//! A sign-in one surface asked for, until it ends: what a dismissal of that surface owes the
//! brain. One mark per connection; plain state, with no GPUI and no transport.

/// What an accepted view shows of the sign-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shows {
    /// It's waiting for the browser or the provider's own window.
    Waiting,
    /// It ended without the brain having shown it waiting: a complete account, or a refused
    /// start.
    Ended,
    /// Anything else, which ends only a sign-in the brain has shown.
    Other,
}

/// What the caller does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Seen {
    Nothing,
    /// The sign-in this surface asked for ended.
    Ended,
    /// Send the owed cancel now, and report it with [`SignInMark::cancel_sent`] once it's sent.
    SendCancel,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SignInMark {
    /// None asked for, or the last one ended.
    #[default]
    Settled,
    /// Asked for on connection `generation`, with no end yet; `shown` once the brain showed it
    /// waiting. It survives a link drop: the next connection shows it again or shows it's gone.
    Open { generation: u64, shown: bool },
    /// Dismissed while open on connection `generation`, and the cancel couldn't be sent.
    CancelOwed { generation: u64 },
}

impl SignInMark {
    /// This surface's sign-in was queued on connection `generation`.
    pub(crate) fn sent(&mut self, generation: u64) {
        *self = Self::Open {
            generation,
            shown: false,
        };
    }

    /// A waiting sign-in this process didn't ask for, after a shell restart, becomes this
    /// surface's own.
    pub(crate) fn adopt(&mut self, generation: u64) {
        if *self == Self::Settled {
            *self = Self::Open {
                generation,
                shown: true,
            };
        }
    }

    /// An accepted view on connection `generation`. An open mark ends only at a terminal event:
    /// a view leaving the wait it showed, one that `Shows::Ended`, or a new connection's first
    /// view that isn't waiting. An owed cancel goes at the next waiting view, and is dropped by a
    /// new connection that shows none.
    pub(crate) fn view(&mut self, generation: u64, shows: Shows) -> Seen {
        match *self {
            Self::Settled => Seen::Nothing,
            Self::Open {
                generation: asked,
                shown,
            } => {
                if shows == Shows::Waiting {
                    *self = Self::Open {
                        generation,
                        shown: true,
                    };
                    Seen::Nothing
                } else if asked == generation && !shown && shows == Shows::Other {
                    Seen::Nothing
                } else {
                    *self = Self::Settled;
                    Seen::Ended
                }
            }
            Self::CancelOwed { generation: asked } => {
                if shows == Shows::Waiting {
                    return Seen::SendCancel;
                }
                if asked != generation {
                    *self = Self::Settled;
                }
                Seen::Nothing
            }
        }
    }

    /// The brain reported the sign-in failed: it has ended, and nothing is owed for it.
    pub(crate) fn failed(&mut self) {
        *self = Self::Settled;
    }

    /// The surface was dismissed: true once, while the sign-in it asked for hasn't ended, and the
    /// cancel is owed until [`Self::cancel_sent`] reports it was sent.
    pub(crate) fn take(&mut self) -> bool {
        let Self::Open { generation, .. } = *self else {
            return false;
        };
        *self = Self::CancelOwed { generation };
        true
    }

    pub(crate) fn cancel_sent(&mut self) {
        if matches!(self, Self::CancelOwed { .. }) {
            *self = Self::Settled;
        }
    }

    /// Onboarding finished: a sign-in still waiting is the rest of the app's, not cancelled.
    pub(crate) fn hand_over(&mut self) {
        if matches!(self, Self::Open { .. }) {
            *self = Self::Settled;
        }
    }

    /// The user sent this connection an op from elsewhere, so the attempt is theirs to steer
    /// there: an owed cancel is dropped.
    pub(crate) fn superseded(&mut self) {
        if matches!(self, Self::CancelOwed { .. }) {
            *self = Self::Settled;
        }
    }
}

#[cfg(test)]
mod tests;
