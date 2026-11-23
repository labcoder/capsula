//! The instance capability in the native credential store, the only place it's stored.

pub use crate::platform::credential::{
    CredentialError, delete_instance as delete, store_instance as store,
};
