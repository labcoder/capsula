//! The default model: the models the brain's catalogs offer, and the newest one picked.

use kumochi_protocol::message::{ConnectionKind, Effort, ModelChoice, ModelInfo, ModelSlug};

use super::{Pending, Settings, SettingsOp, Slot};

impl Settings {
    /// Keeps `connection`'s catalog whole, in its order; an empty one takes it off offer.
    pub fn set_models(&mut self, connection: ConnectionKind, models: Vec<ModelInfo>) {
        self.models.retain(|(kind, _)| *kind != connection);
        // shortcut: only connections with an engine are offered; add AnthropicKey once it has one.
        let served = matches!(
            connection,
            ConnectionKind::ChatGpt | ConnectionKind::ClaudeCode
        );
        if served && !models.is_empty() {
            self.models.push((connection, models));
        }
    }

    /// Every model on offer, catalog by catalog.
    pub fn model_choices(&self) -> impl Iterator<Item = (ConnectionKind, &ModelInfo)> {
        self.models
            .iter()
            .flat_map(|(connection, models)| models.iter().map(move |model| (*connection, model)))
    }

    /// The newest default model the user chose: the one queued, else the saved one.
    pub fn default_model_choice(&self) -> Option<&ModelChoice> {
        self.model_queued
            .as_ref()
            .or_else(|| self.view.as_ref()?.default_model.as_ref())
    }

    /// Whether a default model change waits for the brain's answer.
    pub fn model_saving(&self) -> bool {
        self.model_queued.is_some()
    }

    /// Sends `model` on `connection` as the default, unless it's the newest choice already.
    /// The effort carries over within a connection; on another it starts at `Low`.
    pub fn send_default_model(
        &mut self,
        connection: ConnectionKind,
        model: ModelSlug,
        send: impl FnOnce(SettingsOp) -> bool,
    ) -> bool {
        if self.view.is_none() {
            return false;
        }
        let newest = self.default_model_choice();
        if newest.is_some_and(|choice| choice.connection == connection && choice.model == model) {
            return false;
        }
        let effort = newest
            .filter(|choice| choice.connection == connection)
            .map_or(Effort::Low, |choice| choice.effort);
        let choice = ModelChoice {
            model,
            effort,
            connection,
        };
        if !send(SettingsOp::SetDefaultModel(choice.clone())) {
            return false;
        }
        self.model_queued = Some(choice);
        self.pending.push_back(Pending {
            slot: Slot::Model,
            failed: false,
            reset_draft: false,
        });
        true
    }
}
