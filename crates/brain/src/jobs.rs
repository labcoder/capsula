//! Background jobs: model requests a run's end starts, outside any run (plans/slice-3.md PR 16;
//! docs/development/engines.md, "Background jobs"). The coordinator owns the one job slot and
//! every decision about it; this module holds the pure parts of the title job: its request and
//! the clean-up of what the model answered.

use kumochi_protocol::constants::DISPLAY_NAME;
use kumochi_protocol::message::{Effort, MAX_TITLE_CHARS, ModelChoice, ThreadTitle};

use crate::context::build::fill;
use crate::context::{InputItem, ModelRequest, Role, RunSnapshot};

const TITLE_PROMPT: &str = include_str!("../prompts/title.md");
/// The most of the first ask a title request carries, in characters [guess].
const ASK_CHARS: usize = 2000;
/// The most text a title job takes from the model; more fails the job [guess].
pub(crate) const MAX_OUTPUT: usize = 4 * 1024;

/// The title request for a task whose first ask is `ask`: the title prompt with the run's names,
/// the ask as the only input, no tools, and the task's model at the lowest effort.
/// shortcut: the catalog lists no efforts yet, so the lowest is `Low`; 13b checks the choice
/// against the catalog for runs and jobs alike.
pub(crate) fn title_request(snapshot: &RunSnapshot, ask: &str) -> ModelRequest {
    let template = TITLE_PROMPT
        .lines()
        .filter(|line| snapshot.user.is_some() || !line.contains("{user}"))
        .collect::<Vec<_>>()
        .join("\n");
    let instructions = fill(
        &template,
        &[
            ("{product}", DISPLAY_NAME),
            ("{assistant}", snapshot.assistant.as_str()),
            ("{user}", snapshot.user.as_ref().map_or("", |user| user.as_str())),
        ],
    );
    ModelRequest {
        choice: snapshot.choice.clone().map(|choice| ModelChoice {
            effort: Effort::Low,
            ..choice
        }),
        instructions,
        input: vec![InputItem::Message {
            role: Role::User,
            text: ask.chars().take(ASK_CHARS).collect(),
        }],
        tools: Vec::new(),
    }
}

/// A title from the model's answer: its first line that isn't blank, without control
/// characters, trimmed and cut to [`MAX_TITLE_CHARS`] characters. `None` when nothing is left.
pub(crate) fn clean_title(output: &str) -> Option<ThreadTitle> {
    let line = output.lines().map(str::trim).find(|line| !line.is_empty())?;
    let kept: String = line
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '\u{2028}' | '\u{2029}'))
        .collect();
    let cut: String = kept.trim().chars().take(MAX_TITLE_CHARS).collect();
    ThreadTitle::new(&cut).ok()
}

#[cfg(test)]
mod tests {
    use kumochi_protocol::message::{
        AssistantName, Effort, ModelChoice, ModelSlug, ThreadId, UserName,
    };

    use super::{ASK_CHARS, clean_title, title_request};
    use crate::context::{ConnectionTag, InputItem, Origin, Role, RunSnapshot};

    fn title(output: &str) -> Option<String> {
        clean_title(output).map(|title| title.as_str().to_owned())
    }

    #[test]
    fn a_title_is_one_clean_line_of_at_most_80_characters() {
        let eighty = "t".repeat(80);
        assert_eq!(title(&eighty), Some(eighty.clone()));
        assert_eq!(title(&format!("{eighty}u")), Some(eighty));
        assert_eq!(title(""), None);
        assert_eq!(title(" \n\t\n"), None);
        assert_eq!(title("\u{7}\u{1b}\u{0}"), None);
        assert_eq!(title("\n  Trip plan  \nSecond line"), Some("Trip plan".to_owned()));
        assert_eq!(title("Trip\u{7} plan\u{2028}"), Some("Trip plan".to_owned()));
        let multibyte = format!("{}é", "t".repeat(79));
        assert_eq!(title(&format!("{multibyte}é")), Some(multibyte));
    }

    #[test]
    fn a_title_request_carries_the_names_and_the_ask_at_low_effort() {
        let snapshot = RunSnapshot {
            origin: Origin::Task(ThreadId(3)),
            assistant: AssistantName::new("Zqxassistant").expect("valid"),
            user: Some(UserName::new("Zqxuser").expect("valid")),
            choice: Some(ModelChoice {
                model: ModelSlug::new("gpt-test").expect("valid"),
                effort: Effort::High,
            }),
            connection: ConnectionTag(0),
        };
        let ask = "a".repeat(ASK_CHARS + 1);
        let request = title_request(&snapshot, &ask);
        assert!(request.instructions.contains("Zqxassistant"));
        assert!(request.instructions.contains("Zqxuser"));
        assert!(!request.instructions.contains('{'));
        assert_eq!(request.choice.map(|choice| choice.effort), Some(Effort::Low));
        assert!(request.tools.is_empty());
        assert_eq!(
            request.input,
            vec![InputItem::Message {
                role: Role::User,
                text: "a".repeat(ASK_CHARS),
            }]
        );
        let unnamed = RunSnapshot {
            user: None,
            ..snapshot
        };
        let request = title_request(&unnamed, "Plan a trip");
        assert!(!request.instructions.contains("name is"));
        assert!(!request.instructions.contains('{'));
    }
}
