use super::lifetime::start_cleanup;
use super::{Arc, AtomicBool, Control, Owner, Permit, Transport};

pub(crate) struct FakePlayback(super::Playback);

impl FakePlayback {
    #[cfg(test)]
    pub(crate) fn xrun(&self) {
        super::OutputEvent::latch(&self.0.shared.pending, super::ErrorKind::Xrun);
    }

    #[cfg(test)]
    pub(crate) fn open(control: Arc<Control>, occupied: &'static AtomicBool) -> (Transport, Self) {
        Self::try_open(control, occupied).expect("only one stream may claim the fixture pool")
    }

    pub(crate) fn try_open(
        control: Arc<Control>,
        occupied: &'static AtomicBool,
    ) -> Option<(Transport, Self)> {
        let permit = Permit::claim(occupied)?;
        let cleanup = start_cleanup()?;
        let (submit, playback) = super::pair(super::SOURCE_RATE, 1, control);
        let owner = Owner::new(submit, permit, cleanup);
        Some((Transport(owner), Self(playback)))
    }

    pub(crate) fn fill(&mut self, out: &mut [f32], at: std::time::Duration) {
        self.0.fill_at(out, Some(at));
    }

    pub(crate) fn ready(&self) -> bool {
        self.0.current.is_some() || self.0.ready.len() != 0
    }

    pub(crate) fn handed(&self) -> u64 {
        self.0.control.handed()
    }
}
