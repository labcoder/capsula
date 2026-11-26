//! The item calls one instance publication makes, which each platform implements.

use std::fmt;

use kumochi_protocol::message::CAPABILITY_LEN;

/// The item calls a platform makes. Each blocks, so each runs on its own blocking thread.
pub(crate) trait InstanceStore: Send + Sync + 'static {
    /// A refusal kind, with no content.
    type Error: Copy + fmt::Debug + Send + 'static;
    fn add(&self, key: &[u8; CAPABILITY_LEN]) -> Result<Added, Self::Error>;
    /// Ok only for an existing plain, protected item.
    fn inspect(&self) -> Result<(), Self::Error>;
    /// Replaces the existing item's value alone.
    fn update(&self, key: &[u8; CAPABILITY_LEN]) -> Result<(), Self::Error>;
    fn read(&self, into: &mut [u8; CAPABILITY_LEN]) -> Result<(), Self::Error>;
    fn delete(&self) -> Result<(), Self::Error>;
}

/// What an add found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Added {
    Fresh,
    Duplicate,
}
