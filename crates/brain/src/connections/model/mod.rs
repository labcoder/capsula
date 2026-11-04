//! Model requests on the signed-in account's plan: the request's HTTP/1.1 framing, the live-check
//! budget, the Responses event stream and what its failures mean. See
//! docs/development/responses.md.

pub mod budget;
pub mod error;
pub mod sse;
pub mod wire;
