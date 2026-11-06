//! Brain-local account records; no credentials are part of the shell protocol.

pub mod account;
pub mod anthropic;
pub(crate) mod authority;
pub mod catalog;
pub mod clock;
pub mod error;
pub mod github;
pub mod http_client;
pub mod marker;
pub mod model;
pub mod oauth;
pub mod records;
pub mod vault;
