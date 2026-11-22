use super::lifetime::start_cleanup;
use super::{Arc, AtomicBool, Control, Owner, Permit, Transport};

pub(crate) struct FakePlayback(super::Playback);

impl FakePlayback {
    #[cfg(test)]
    /// Reports an underrun and wakes its worker, as the native error callback does.
    pub(crate) fn xrun(&self) {
        super::OutputEvent::latch(&self.0.shared.pending, super::ErrorKind::Xrun);
        self.0.control.wake();
    }

    /// Reports the stream lost and wakes its worker, as the native error callback does.
    pub(crate) fn lose(&self) {
        super::OutputEvent::latch(&self.0.shared.pending, super::ErrorKind::StreamInvalidated);
        self.0.control.wake();
    }

    #[cfg(test)]
    pub(crate) fn force_observation_overflow(&self) {
        self.0.control.overflow();
    }

    #[cfg(test)]
    pub(crate) fn fill_during_copy(
        &mut self,
        out: &mut [f32],
        at: std::time::Duration,
        action: impl FnOnce(&Control),
    ) {
        let mut action = Some(action);
        self.0.control.note_callback();
        self.0.render(
            out,
            Some(at),
            |control| {
                control.begin_copy()?;
                if let Some(action) = action.take() {
                    action(control);
                }
                Ok(())
            },
            || {},
        );
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
