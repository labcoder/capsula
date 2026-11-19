//! One verified authority's fixed, bounded Anthropic transport.

use std::num::NonZeroU64;
use std::sync::Arc;

use kumochi_protocol::Secret;
use kumochi_protocol::constants::{LIVE_KEY_CATALOG_BUDGET_ENV, LIVE_KEY_MESSAGES_BUDGET_ENV};

use super::error::Failure;
use super::fence::{Fence, Fenced};
use super::key::AnthropicKey;
use super::messages::{self, MessagesBody};
use super::models::{self, CatalogModel, CheckError};
use super::sse_map::{Capture, StopReason};
use super::tls::{AnthropicTls, RouteConnector};
use crate::connections::model::Events;
use crate::connections::model::budget::Budget;
use crate::connections::model::conn::ModelClient;
use crate::context::ConnectionTag;

const SERVE_ANTHROPIC_KEY: bool = false;

#[derive(Clone)]
pub(crate) struct Transport {
    connector: RouteConnector,
    catalog: Arc<Budget>,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used at the next layer's engine dispatch")
    )]
    messages: Arc<Budget>,
}

impl Transport {
    pub(crate) fn native() -> Option<Self> {
        if !SERVE_ANTHROPIC_KEY {
            return None;
        }
        Some(Self {
            connector: RouteConnector::Native(AnthropicTls::build().ok().map(Arc::new)),
            catalog: Arc::new(Budget::from_var(LIVE_KEY_CATALOG_BUDGET_ENV)),
            messages: Arc::new(Budget::from_var(LIVE_KEY_MESSAGES_BUDGET_ENV)),
        })
    }

    #[cfg(test)]
    pub(crate) fn offline(peer: super::tls::Scripted, catalog: u32, messages: u32) -> Self {
        Self {
            connector: RouteConnector::Offline(peer),
            catalog: Arc::new(Budget::limited(Some(catalog))),
            messages: Arc::new(Budget::limited(Some(messages))),
        }
    }

    pub(crate) async fn catalog(
        &self,
        fence: &Fence,
        key: &Secret<AnthropicKey>,
    ) -> Result<Vec<CatalogModel>, CheckError> {
        fence
            .while_open(async {
                if !self.catalog.reserve() {
                    return Err(CheckError::Unconfirmed);
                }
                let connect = Fenced(self.connector.clone(), fence.clone());
                models::read_fenced(&connect, key).await
            })
            .await
            .unwrap_or(Err(CheckError::Unconfirmed))
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the route is not yet dispatched in production")
    )]
    pub(crate) async fn messages<L: Send>(
        &self,
        authority: &Verified,
        offer: Offer<'_>,
        grant: impl Future<Output = Option<L>> + Send,
        capture: Capture,
        out: impl Events + Send,
    ) -> Result<StopReason, Failure> {
        if offer.tag != authority.tag {
            return Err(Failure::Credential);
        }
        let client = ModelClient::new(Fenced(
            Fenced(self.connector.clone(), offer.fence.clone()),
            authority.fence.clone(),
        ));
        authority
            .fence
            .while_open(offer.fence.while_open(messages::stream(
                &client,
                offer.body,
                &authority.key,
                grant,
                offer.budget,
                &self.messages,
                capture,
                out,
            )))
            .await
            .flatten()
            .unwrap_or(Err(Failure::Transport))
    }
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the route is not yet dispatched in production")
)]
pub(crate) struct Offer<'a> {
    pub(crate) tag: ConnectionTag,
    pub(crate) body: &'a MessagesBody,
    pub(crate) fence: &'a Fence,
    pub(crate) budget: &'a Budget,
}

pub(crate) struct Verified {
    pub(crate) revision: u64,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used at the next layer's engine dispatch")
    )]
    key: Secret<AnthropicKey>,
    pub(crate) tag: ConnectionTag,
    pub(crate) fence: Fence,
}

impl Verified {
    pub(crate) fn new(revision: u64, key: Secret<AnthropicKey>) -> Option<Self> {
        Self::from_entropy(revision, key, getrandom::u64().ok())
    }

    fn from_entropy(
        revision: u64,
        key: Secret<AnthropicKey>,
        entropy: Option<u64>,
    ) -> Option<Self> {
        let tag = ConnectionTag(NonZeroU64::new(entropy?)?.get());
        Some(Self {
            revision,
            key,
            tag,
            fence: Fence::default(),
        })
    }
}

#[cfg(test)]
mod tests;
