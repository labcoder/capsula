//! Never-published local exclusion; this owner cannot authorize a capture job.

use kumochi_shell_core::presence::exclusion::machine;

use gpui_kit::AnyWindowHandle;
use kumochi_protocol::message::CaptureFor;

use super::{Context, Presence, platform};
use machine::Surfaces;
pub(super) use machine::{Error, Exclusion, Slot};

pub(super) enum Receipt {
    Windows {
        window: Option<AnyWindowHandle>,
        affinity: platform::Affinity,
    },
    #[cfg(test)]
    Fixture {
        window: Option<AnyWindowHandle>,
        prior: machine::fixture::Prior,
    },
}

#[cfg(test)]
pub(super) use machine::fixture::Fake as Fixture;

pub(super) struct Windows<'a> {
    pub(super) native: &'a platform::PresenceNative,
    pub(super) handles: [Option<AnyWindowHandle>; 8],
    pub(super) cx: &'a mut gpui_kit::App,
    #[cfg(test)]
    pub(super) fixture: Option<std::rc::Rc<std::cell::RefCell<Fixture>>>,
}

impl Windows<'_> {
    #[cfg_attr(
        test,
        expect(
            clippy::panic_in_result_fn,
            reason = "fixture assertions enforce exclusion before native placement"
        )
    )]
    pub(super) fn protect(
        &mut self,
        owner: &mut Exclusion<Receipt>,
        slot: Slot,
    ) -> Result<(), Error> {
        owner.attach(slot, self)?;
        #[cfg(test)]
        if let (Some(lease), Some(fixture)) = (&owner.lease, &self.fixture) {
            assert!(!lease.revoked, "a revoked owner cannot reach placement");
            fixture.borrow().assert_excluded(slot);
        }
        Ok(())
    }
}

#[cfg(test)]
impl Windows<'_> {
    fn with_fixture<T>(
        &mut self,
        window: Option<AnyWindowHandle>,
        apply: impl FnOnce(&mut Fixture) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let fixture = self.fixture.as_ref().ok_or(Error::Surface)?;
        match window {
            Some(window) => window
                .update(self.cx, |_, _, _| apply(&mut fixture.borrow_mut()))
                .map_err(|_| Error::Surface)?,
            None => apply(&mut fixture.borrow_mut()),
        }
    }
}

impl Surfaces for Windows<'_> {
    type Receipt = Receipt;

    fn snapshot(&mut self, slot: Slot) -> Result<Option<Receipt>, Error> {
        let window = self.handles[slot as usize];
        if window.is_none() && !matches!(slot, Slot::Proxy) {
            return Ok(None);
        }
        #[cfg(test)]
        if self.fixture.is_some() {
            return self
                .with_fixture(window, |fixture| fixture.snapshot(slot))
                .map(|prior| prior.map(|prior| Receipt::Fixture { window, prior }));
        }
        let affinity = match window {
            None => self.native.affinity(),
            Some(window) => window
                .update(self.cx, |_, window, _| platform::window_affinity(window))
                .map_err(|_| Error::Surface)?,
        }
        .map_err(|_| Error::Surface)?;
        Ok(Some(Receipt::Windows { window, affinity }))
    }

    fn exclude(&mut self, slot: Slot, receipt: &Receipt) -> Result<(), Error> {
        #[cfg(not(test))]
        let _ = slot;
        match receipt {
            Receipt::Windows { window, affinity } => match window {
                None => self.native.exclude(affinity),
                Some(window) => window
                    .update(self.cx, |_, window, _| {
                        platform::exclude_window(window, affinity)
                    })
                    .map_err(|_| Error::Surface)?,
            }
            .map_err(|_| Error::Surface),
            #[cfg(test)]
            Receipt::Fixture { window, prior } => {
                self.with_fixture(*window, |fixture| fixture.exclude(slot, prior))
            }
        }
    }

    fn restore(&mut self, slot: Slot, receipt: &Receipt) -> Result<(), Error> {
        #[cfg(not(test))]
        let _ = slot;
        if Self::gone(receipt) {
            return Ok(());
        }
        match receipt {
            Receipt::Windows { window, affinity } => match window {
                None => self.native.restore_affinity(affinity),
                Some(window) => window
                    .update(self.cx, |_, window, _| {
                        platform::restore_window_affinity(window, affinity)
                    })
                    .map_err(|_| Error::Surface)?,
            }
            .map_err(|_| Error::Surface),
            #[cfg(test)]
            Receipt::Fixture { window, prior } => {
                self.with_fixture(*window, |fixture| fixture.restore(slot, prior))
            }
        }
    }

    fn gone(receipt: &Receipt) -> bool {
        match receipt {
            Receipt::Windows { affinity, .. } => affinity.gone(),
            #[cfg(test)]
            Receipt::Fixture { prior, .. } => Fixture::gone(prior),
        }
    }

    fn same(&self, slot: Slot, receipt: &Receipt) -> bool {
        match receipt {
            Receipt::Windows { window, .. } => self.handles[slot as usize] == *window,
            #[cfg(test)]
            Receipt::Fixture { window, prior } => {
                self.handles[slot as usize] == *window
                    && self
                        .fixture
                        .as_ref()
                        .is_some_and(|fixture| fixture.borrow().same(slot, prior))
            }
        }
    }
}

impl Presence {
    fn with_exclusion<T>(
        &mut self,
        cx: &mut Context<'_, Self>,
        apply: impl FnOnce(&mut Exclusion<Receipt>, &mut Windows<'_>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let handles = self.exclusion_handles();
        apply(
            &mut self.exclusion,
            &mut Windows {
                native: &self.native,
                handles,
                #[cfg(test)]
                fixture: self.exclusion_fixture.clone(),
                cx,
            },
        )
    }

    pub(super) fn exclusion_handles(&self) -> [Option<AnyWindowHandle>; 8] {
        [
            Some(self.dock_window),
            None,
            Some(self.panel_window),
            self.text.as_ref().map(|panel| panel.window),
            self.thread_panel.as_ref().map(|panel| panel.window),
            self.settings_panel.as_ref().map(|panel| panel.window),
            self.overlay.window().map(|overlay| overlay.window),
            self.onboarding.as_ref().map(|panel| panel.window),
        ]
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "never published: no production acquisition caller until coordinator and native retirement are integrated"
        )
    )]
    fn acquire_exclusion(
        &mut self,
        session: &crate::link::Session,
        input: CaptureFor,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), Error> {
        if self
            .session
            .as_ref()
            .is_none_or(|current| current.generation() != session.generation())
        {
            return Err(Error::Busy);
        }
        let now = cx.background_executor().now();
        let key = self.with_exclusion(cx, |owner, surfaces| {
            owner.acquire(session.generation(), input, now, surfaces)
        })?;
        let deadline = self
            .exclusion
            .lease
            .as_ref()
            .expect("acquired above")
            .deadline;
        self.exclusion_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(deadline.saturating_duration_since(cx.background_executor().now()))
                .await;
            let _owner = this.update(cx, |this: &mut Self, cx| {
                let now = cx.background_executor().now();
                if this
                    .with_exclusion(cx, |owner, surfaces| owner.expire(key, now, surfaces))
                    .is_err()
                {
                    tracing::warn!("exclusion cleanup retained restoration debt");
                }
            });
        }));
        Ok(())
    }

    pub(super) fn protect_surface(
        &mut self,
        slot: Slot,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), Error> {
        self.with_exclusion(cx, |owner, surfaces| surfaces.protect(owner, slot))
    }

    pub(super) fn revoke_exclusion(&mut self, cx: &mut Context<'_, Self>) {
        if self
            .with_exclusion(cx, |owner, surfaces| owner.cleanup(surfaces))
            .is_err()
        {
            tracing::warn!("exclusion cleanup retained restoration debt");
        }
    }

    pub(super) fn capture_finished(
        &mut self,
        session: u64,
        input: CaptureFor,
        cx: &mut Context<'_, Self>,
    ) {
        if self.exclusion.matches(session, input) {
            self.revoke_exclusion(cx);
        }
    }
}

#[cfg(test)]
#[path = "exclusion_tests.rs"]
mod tests;
