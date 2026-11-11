//! Identity and execution control retained across an admitted page read.

use crate::recall::control::Control;

/// One admitted page's identity and bounded execution capability.
#[derive(Clone, Debug)]
pub struct PageKey {
    pub(crate) epoch: u64,
    pub(crate) request: u32,
    pub(crate) token: u64,
    pub(crate) control: Control,
}

impl PartialEq for PageKey {
    fn eq(&self, other: &Self) -> bool {
        (self.epoch, self.request, self.token) == (other.epoch, other.request, other.token)
    }
}

impl Eq for PageKey {}

#[cfg(test)]
impl PageKey {
    pub(crate) fn for_tests(epoch: u64, request: u32) -> Self {
        Self {
            epoch,
            request,
            token: u64::from(request),
            control: Control::new(std::time::Instant::now() + std::time::Duration::from_secs(60)),
        }
    }
}
