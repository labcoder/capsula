use kumochi_protocol::message::{ConnectionKind, DockPosition, MaxFrameRate, ModelInfo, ModelSlug};

use super::Settings;
use crate::strings::{Strings, Text};

impl Settings {
    /// The picker rows (connection, slug, label) and checked index. A default absent from the
    /// catalogs comes first, named by its slug.
    pub fn default_model_options(
        &self,
        strings: &Strings,
    ) -> (Vec<(ConnectionKind, ModelSlug, String)>, Option<usize>) {
        let offered: Vec<_> = self.model_choices().collect();
        let mut choices: Vec<_> = offered
            .iter()
            .map(|&(connection, model)| {
                (
                    connection,
                    model.slug.clone(),
                    label(connection, model.display_name.as_str(), &offered, strings),
                )
            })
            .collect();
        let selected = self.default_model_choice().map(|current| {
            if let Some(index) = offered.iter().position(|&(connection, model)| {
                connection == current.connection && model.slug == current.model
            }) {
                index
            } else {
                choices.insert(
                    0,
                    (
                        current.connection,
                        current.model.clone(),
                        label(current.connection, current.model.as_str(), &[], strings),
                    ),
                );
                0
            }
        });
        (choices, selected)
    }

    /// The frame rate radios in order: Automatic, then every cap.
    pub fn frame_rate_options() -> impl Iterator<Item = MaxFrameRate> {
        std::iter::once(MaxFrameRate::AUTOMATIC).chain(
            MaxFrameRate::CAPS
                .into_iter()
                .filter_map(|fps| MaxFrameRate::cap(fps).ok()),
        )
    }

    /// The radio showing the newest choice, queued or saved: Kit gives a checked radio no click
    /// handler, so showing the saved cap while a new one is queued would swallow a quick reversal.
    pub fn frame_rate_index(&self) -> usize {
        self.max_frame_rate_choice()
            .and_then(|choice| Self::frame_rate_options().position(|rate| rate == choice))
            .unwrap_or_default()
    }

    /// The cap behind the radio at this index, if it exists.
    pub fn frame_rate_at(index: usize) -> Option<MaxFrameRate> {
        Self::frame_rate_options().nth(index)
    }

    /// The dock position cells in grid order, with their caption tokens.
    pub fn dock_position_options() -> impl Iterator<Item = (DockPosition, Text)> {
        DockPosition::ALL.into_iter().zip(Text::POSITIONS)
    }

    /// The checked dock position cell, if there is a choice.
    pub fn dock_position_index(&self) -> Option<usize> {
        let choice = self.dock_position_choice()?;
        Self::dock_position_options().position(|(position, _)| position == choice)
    }
}

/// A key model's row always names the key, since each request is billed; another row names its
/// connection only when another connection offers a model of the same name.
fn label(
    connection: ConnectionKind,
    name: &str,
    offered: &[(ConnectionKind, &ModelInfo)],
    strings: &Strings,
) -> String {
    let shared = offered
        .iter()
        .any(|(other, model)| *other != connection && model.display_name.as_str() == name);
    let provider = match connection {
        ConnectionKind::AnthropicKey => Text::AnthropicTitle,
        _ if !shared => return name.to_owned(),
        ConnectionKind::ChatGpt => Text::AccountProvider,
        ConnectionKind::ClaudeCode => Text::ClaudeProvider,
    };
    strings.model_on(name, &strings.get(provider))
}

#[cfg(test)]
mod tests;
