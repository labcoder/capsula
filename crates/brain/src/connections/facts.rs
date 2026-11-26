//! What the brain knows about each connection kind that never changes while it runs, in one
//! exhaustive table (docs/development/runs.md, "Connection facts"). A fact describes a kind and
//! grants nothing: routes, sign-ins, catalogs, offers, consent and pictures are checked where
//! they live.

use std::ops::{Index, IndexMut};

use kumochi_protocol::message::ConnectionKind;

const COUNT: usize = 3;

/// Every kind, in the order of [`Facts::index`].
pub(crate) const KINDS: [ConnectionKind; COUNT] = [
    ConnectionKind::ChatGpt,
    ConnectionKind::AnthropicKey,
    ConnectionKind::ClaudeCode,
];

/// How a run's stored rounds replay on a kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Replay {
    /// Native items, which carry their round's text.
    Carried,
    /// Native tool calls only, beside their round's text.
    Calls,
    /// Text only.
    Text,
}

/// One kind's fixed facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Facts {
    /// Its slot in per-kind state.
    pub(crate) index: usize,
    /// Its spelling in saved choices and stored rounds, which written rows keep for good.
    pub(crate) stored: &'static str,
    /// How its stored rounds replay.
    pub(crate) replay: Replay,
    /// Whether shipped builds let its requests carry a picture at all. Only this row switches a
    /// route on; the model, consent and screen-grant checks still decide each picture.
    pub(crate) pictures: bool,
}

/// `kind`'s facts.
pub(crate) const fn of(kind: ConnectionKind) -> Facts {
    match kind {
        ConnectionKind::ChatGpt => Facts {
            index: 0,
            stored: "chatgpt",
            replay: Replay::Carried,
            pictures: true,
        },
        ConnectionKind::AnthropicKey => Facts {
            index: 1,
            stored: "anthropic_key",
            replay: Replay::Calls,
            pictures: true,
        },
        ConnectionKind::ClaudeCode => Facts {
            index: 2,
            stored: "claude_code",
            replay: Replay::Text,
            pictures: false,
        },
    }
}

/// The kind a stored spelling names; none for any other text.
pub(crate) fn stored(text: &str) -> Option<ConnectionKind> {
    KINDS.into_iter().find(|kind| of(*kind).stored == text)
}

/// One value per kind, indexed by kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PerKind<T>([T; COUNT]);

impl<T> PerKind<T> {
    /// Each kind's value from `value`.
    pub(crate) fn new(value: impl FnMut(ConnectionKind) -> T) -> Self {
        Self(KINDS.map(value))
    }
}

impl<T> Index<ConnectionKind> for PerKind<T> {
    type Output = T;

    fn index(&self, kind: ConnectionKind) -> &T {
        &self.0[of(kind).index]
    }
}

impl<T> IndexMut<ConnectionKind> for PerKind<T> {
    fn index_mut(&mut self, kind: ConnectionKind) -> &mut T {
        &mut self.0[of(kind).index]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_keeps_its_slot_spelling_replay_and_picture_switch() {
        for (kind, index, spelling, replay, pictures) in [
            (ConnectionKind::ChatGpt, 0, "chatgpt", Replay::Carried, true),
            (
                ConnectionKind::AnthropicKey,
                1,
                "anthropic_key",
                Replay::Calls,
                true,
            ),
            (
                ConnectionKind::ClaudeCode,
                2,
                "claude_code",
                Replay::Text,
                false,
            ),
        ] {
            assert_eq!(
                of(kind),
                Facts {
                    index,
                    stored: spelling,
                    replay,
                    pictures,
                }
            );
            assert_eq!(KINDS[index], kind);
            assert_eq!(stored(spelling), Some(kind));
        }
        for text in ["", "ChatGpt", "chatgpt ", "CLAUDE_CODE", "anthropic"] {
            assert_eq!(stored(text), None, "{text:?}");
        }
        let mut slots = PerKind::new(|kind| of(kind).index * 10);
        slots[ConnectionKind::AnthropicKey] = 7;
        assert_eq!(slots, PerKind([0, 7, 20]));
    }
}
