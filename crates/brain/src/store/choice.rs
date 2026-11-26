//! Saved model identity: structural decoding is independent of display and execution admission.

use kumochi_protocol::message::{ConnectionKind, Effort, ModelChoice, ModelSlug};

use super::StoreError;
use crate::connections::facts;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StoredChoice {
    Absent,
    Valid {
        connection: ConnectionKind,
        model: ModelSlug,
        effort: Option<Effort>,
    },
    Invalid,
}

impl StoredChoice {
    pub(crate) fn decode(
        model: Option<&str>,
        effort: Option<&str>,
        connection: Option<&str>,
    ) -> Self {
        if (model, effort, connection) == (None, None, None) {
            return Self::Absent;
        }
        let (Some(model), Some(effort), Some(connection)) = (model, effort, connection) else {
            return Self::Invalid;
        };
        let Ok(model) = ModelSlug::new(model) else {
            return Self::Invalid;
        };
        let Some(connection) = facts::stored(connection) else {
            return Self::Invalid;
        };
        Self::Valid {
            connection,
            model,
            effort: Effort::ALL
                .into_iter()
                .find(|known| effort_text(*known) == effort),
        }
    }

    pub(crate) fn display(self) -> rusqlite::Result<Option<ModelChoice>> {
        match self {
            Self::Absent => Ok(None),
            Self::Invalid => Err(rusqlite::Error::InvalidQuery),
            Self::Valid {
                connection,
                model,
                effort,
            } => Ok(Some(ModelChoice {
                connection,
                model,
                effort: effort.unwrap_or(Effort::Medium),
            })),
        }
    }

    /// The choice a run executes: the saved one on any connection, else `default`; admission
    /// decides whether its connection can answer.
    pub(crate) fn execution(
        self,
        default: Option<ModelChoice>,
    ) -> Result<Option<ModelChoice>, StoreError> {
        let choice = match self {
            Self::Absent => default,
            Self::Invalid => return Err(StoreError::Read),
            Self::Valid {
                connection,
                model,
                effort,
            } => Some(ModelChoice {
                connection,
                model,
                effort: effort.unwrap_or(Effort::Low),
            }),
        };
        Ok(choice)
    }
}

pub(super) fn connection_text(connection: ConnectionKind) -> &'static str {
    facts::of(connection).stored
}

pub(super) fn effort_text(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "xhigh",
        Effort::Max => "max",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_three_unset_fields_are_absent() {
        assert_eq!(StoredChoice::decode(None, None, None), StoredChoice::Absent);
        for (model, effort, connection) in [
            (Some("same-slug"), None, None),
            (None, Some("high"), None),
            (None, None, Some("chatgpt")),
            (Some("same-slug"), Some("high"), None),
            (Some("same-slug"), None, Some("chatgpt")),
            (None, Some("high"), Some("chatgpt")),
            (Some("same-slug"), Some("high"), Some("unknown")),
            (Some("Not A Slug"), Some("high"), Some("chatgpt")),
            (Some(""), Some("high"), Some("chatgpt")),
        ] {
            let choice = StoredChoice::decode(model, effort, connection);
            assert_eq!(choice, StoredChoice::Invalid);
            assert!(choice.clone().display().is_err());
            assert!(matches!(
                choice.execution(Some(default())),
                Err(StoreError::Read)
            ));
        }
        assert_eq!(
            StoredChoice::Absent
                .execution(Some(default()))
                .expect("default"),
            Some(default())
        );
    }

    fn default() -> ModelChoice {
        ModelChoice {
            connection: ConnectionKind::ChatGpt,
            model: ModelSlug::new("different-model").expect("slug"),
            effort: Effort::High,
        }
    }

    #[test]
    fn unknown_effort_never_selects_a_different_model() {
        for effort in ["minimal", "ultra", ""] {
            let stored = StoredChoice::decode(Some("same-slug"), Some(effort), Some("chatgpt"));
            let shown = stored
                .clone()
                .display()
                .expect("display")
                .expect("selected");
            assert_eq!(
                shown,
                ModelChoice {
                    connection: ConnectionKind::ChatGpt,
                    model: ModelSlug::new("same-slug").expect("slug"),
                    effort: Effort::Medium,
                }
            );
            assert_eq!(
                stored.execution(Some(default())).expect("execute"),
                Some(ModelChoice {
                    effort: Effort::Low,
                    ..shown
                })
            );
        }
    }

    #[test]
    fn a_saved_route_is_shown_and_read_as_saved() {
        for connection in ["claude_code", "anthropic_key"] {
            let stored = StoredChoice::decode(Some("same-slug"), Some("ultra"), Some(connection));
            let shown = stored.clone().display().expect("page").expect("selected");
            assert_ne!(shown.connection, ConnectionKind::ChatGpt);
            assert_eq!(shown.model.as_str(), "same-slug");
            assert_eq!(shown.effort, Effort::Medium);
            assert_eq!(
                stored.execution(Some(default())).expect("read"),
                Some(ModelChoice {
                    effort: Effort::Low,
                    ..shown.clone()
                })
            );
            assert_eq!(
                StoredChoice::Absent
                    .execution(Some(shown.clone()))
                    .expect("read"),
                Some(shown)
            );
        }
    }
}
