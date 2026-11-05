//! Brain-local account records; no credentials are part of the shell protocol.

pub mod account;
pub mod anthropic;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "OSC-233 PR 2 moves the vault onto the authority")
)]
pub(crate) mod authority;
pub mod catalog;
pub mod clock;
pub mod error;
pub mod http_client;
pub mod marker;
pub mod model;
pub mod oauth;
pub mod records;
pub mod vault;
