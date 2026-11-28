//! The model transport's fence around an Anthropic connector: each connect poll runs inside it,
//! as each I/O poll of the connection it opens does.

use std::future::poll_fn;
use std::io;
use std::pin::pin;
use std::sync::Arc;

use super::tls::{AnthropicConnect, sealed};
use crate::connections::model::conn::Connect;
pub(crate) use crate::connections::model::fence::{Fence, Fenced};
use crate::platform::connect_with_cut;

impl<C: AnthropicConnect> Connect for Fenced<C> {
    type Io = Fenced<C::Io>;

    async fn connect(&self) -> io::Result<Self::Io> {
        let mut opening = pin!(
            self.1
                .with(|| { connect_with_cut(Arc::clone(self.1.cut()), self.0.connect()) })?
        );
        let io = poll_fn(|cx| self.1.with(|| opening.as_mut().poll(cx))?).await?;
        Ok(Fenced(io, self.1.clone()))
    }
}

impl<C: AnthropicConnect> sealed::Sealed for Fenced<C> {}
impl<C: AnthropicConnect> AnthropicConnect for Fenced<C> {}

#[cfg(test)]
mod tests;
