//! The model server on this computer the local route answers on: its endpoint, model and
//! generation tag, and the fence every engine built for it holds. Nothing in a shipped build sets
//! it while [`LOCAL_CHAT`] is off; tests set it in the order the settings writer will use, which
//! retires the old server before the new one is reported (docs/development/engines.md, "The local
//! engine").
//!
//! [`LOCAL_CHAT`]: kumochi_protocol::constants::LOCAL_CHAT

use kumochi_protocol::message::ModelSlug;

use super::Brain;
use crate::connections::local::endpoint::LoopbackEndpoint;
use crate::connections::model::budget::Budget;
use crate::connections::model::fence::Fence;
use crate::context::ConnectionTag;
use crate::engines::local::LocalEngine;
use crate::engines::{self, Engine, RunRequest};

/// One configured local server. A new endpoint or model is a new server under a fresh tag.
#[derive(Debug)]
pub(super) struct LocalServer {
    endpoint: LoopbackEndpoint,
    model: ModelSlug,
    tag: ConnectionTag,
    pub(super) fence: Fence,
}

impl Brain {
    /// A local run's engine: only on the server whose tag the run's route carries, so a run never
    /// reaches a server that replaced its own.
    pub(super) fn local_engine(&self, request: &RunRequest) -> Box<dyn Engine> {
        self.local_engine_on(self.coordinator.local_tag(request))
    }

    /// The engine for a local run admitted under `tag`: a real one only while the server here is
    /// still that tag's, so a run admitted before a replacement was heard never reaches the new one.
    pub(super) fn local_engine_on(&self, tag: Option<ConnectionTag>) -> Box<dyn Engine> {
        match self.local.as_ref().filter(|server| Some(server.tag) == tag) {
            Some(server) => Box::new(LocalEngine::new(
                server.endpoint,
                server.model.clone(),
                Budget::default(),
                server.fence.clone(),
            )),
            None => Box::new(engines::Unserved::model_unavailable()),
        }
    }
}

#[cfg(test)]
impl Brain {
    /// Replaces the local server, or removes it with `None`: the old one's fence is revoked
    /// first, so nothing built for it connects, writes or reads again, and only then is the
    /// change reported, its hold fence applied before this returns.
    pub(super) fn set_local_server(
        &mut self,
        server: Option<(LoopbackEndpoint, ModelSlug, ConnectionTag)>,
    ) -> Result<(), super::Full> {
        use kumochi_protocol::message::{ConnectionKind, ErrorKey, ModelInfo, ModelName};

        use crate::coordinator::{Availability, Input, Ready};

        if let Some(old) = self.local.take() {
            old.fence.revoke();
        }
        let now = server.as_ref().map_or_else(
            || Availability::Unavailable(ErrorKey::LocalOff.into()),
            |(_, model, tag)| {
                Availability::Ready(Ready {
                    connection: *tag,
                    models: vec![ModelInfo {
                        slug: model.clone(),
                        display_name: ModelName::new(model.as_str()).expect("a slug is a name"),
                        supported_efforts: None,
                    }],
                    images: Vec::new(),
                    trusted_draw: false,
                })
            },
        );
        self.local = server.map(|(endpoint, model, tag)| LocalServer {
            endpoint,
            model,
            tag,
            fence: Fence::default(),
        });
        let input = Input::Engines(ConnectionKind::LocalChat, now);
        self.hold_fence(&input);
        let effects = self
            .coordinator
            .handle(input)
            .expect("availability is never stale");
        self.apply(effects, None).map_err(|_| super::Full)
    }
}
