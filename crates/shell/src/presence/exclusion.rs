//! Never-published local exclusion; this owner cannot authorize a capture job.

use std::time::{Duration, Instant};

use gpui_kit::AnyWindowHandle;
use kumochi_protocol::message::{CaptureFor, LeaseGeneration};

use super::{Context, Presence, platform};

const LIFETIME: Duration = Duration::from_secs(3);
const SLOTS: [Slot; 7] = [
    Slot::Dock,
    Slot::Proxy,
    Slot::Status,
    Slot::Text,
    Slot::Threads,
    Slot::Settings,
    Slot::Drawing,
];

#[derive(Clone, Copy, Debug)]
pub(super) enum Slot {
    Dock,
    Proxy,
    Status,
    Text,
    Threads,
    Settings,
    Drawing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
    session: u64,
    input: CaptureFor,
    generation: LeaseGeneration,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum Error {
    #[error("exclusion is busy or has restoration debt")]
    Busy,
    #[error("exclusion generation or deadline exhausted")]
    Exhausted,
    #[error("a presence surface could not preserve its affinity")]
    Surface,
}

trait Surfaces {
    type Receipt;
    fn snapshot(&mut self, slot: Slot) -> Result<Option<Self::Receipt>, Error>;
    fn exclude(&mut self, slot: Slot, receipt: &Self::Receipt) -> Result<(), Error>;
    fn restore(&mut self, slot: Slot, receipt: &Self::Receipt) -> Result<(), Error>;
    fn gone(receipt: &Self::Receipt) -> bool;
    fn same(&self, slot: Slot, receipt: &Self::Receipt) -> bool;
}

struct ExclusionLease<R> {
    key: Key,
    deadline: Instant,
    revoked: bool,
    receipts: [Option<R>; 7],
}

pub(super) struct Exclusion<R> {
    generation: u32,
    lease: Option<ExclusionLease<R>>,
}

impl<R> Default for Exclusion<R> {
    fn default() -> Self {
        Self {
            generation: 0,
            lease: None,
        }
    }
}

impl<R> Exclusion<R> {
    fn acquire(
        &mut self,
        session: u64,
        input: CaptureFor,
        now: Instant,
        surfaces: &mut impl Surfaces<Receipt = R>,
    ) -> Result<Key, Error> {
        if self.lease.is_some() {
            return Err(Error::Busy);
        }
        self.generation = self.generation.checked_add(1).ok_or(Error::Exhausted)?;
        let deadline = now.checked_add(LIFETIME).ok_or(Error::Exhausted)?;
        let key = Key {
            session,
            input,
            generation: LeaseGeneration(self.generation),
        };
        let mut receipts = std::array::from_fn(|_| None);
        for slot in SLOTS {
            receipts[slot as usize] = surfaces.snapshot(slot)?;
        }
        self.lease = Some(ExclusionLease {
            key,
            deadline,
            revoked: false,
            receipts,
        });
        for slot in SLOTS {
            let receipt = &self.lease.as_ref().expect("installed above").receipts[slot as usize];
            if let Some(receipt) = receipt
                && surfaces.exclude(slot, receipt).is_err()
            {
                self.cleanup(surfaces)?;
                return Err(Error::Surface);
            }
        }
        Ok(key)
    }

    fn attach<S: Surfaces<Receipt = R>>(
        &mut self,
        slot: Slot,
        surfaces: &mut S,
    ) -> Result<(), Error> {
        let Some(lease) = &mut self.lease else {
            return Ok(());
        };
        if lease.revoked {
            return Err(Error::Busy);
        }
        let receipt = &mut lease.receipts[slot as usize];
        if let Some(prior) = receipt.as_ref().filter(|prior| !S::gone(prior)) {
            if surfaces.same(slot, prior) {
                return Ok(());
            }
            self.cleanup(surfaces)?;
            return Err(Error::Surface);
        }
        let result = surfaces.snapshot(slot).and_then(|prior| {
            *receipt = prior;
            let prior = receipt.as_ref().ok_or(Error::Surface)?;
            surfaces.exclude(slot, prior)
        });
        if result.is_err() {
            self.cleanup(surfaces)?;
        }
        result
    }

    fn cleanup(&mut self, surfaces: &mut impl Surfaces<Receipt = R>) -> Result<(), Error> {
        let Some(lease) = &mut self.lease else {
            return Ok(());
        };
        lease.revoked = true;
        for slot in SLOTS {
            let receipt = &mut lease.receipts[slot as usize];
            if let Some(prior) = receipt
                && surfaces.restore(slot, prior).is_ok()
            {
                *receipt = None;
            }
        }
        if lease.receipts.iter().any(Option::is_some) {
            return Err(Error::Surface);
        }
        self.lease = None;
        Ok(())
    }

    fn expire(
        &mut self,
        key: Key,
        now: Instant,
        surfaces: &mut impl Surfaces<Receipt = R>,
    ) -> Result<(), Error> {
        if self
            .lease
            .as_ref()
            .is_some_and(|lease| lease.key == key && now >= lease.deadline)
        {
            self.cleanup(surfaces)
        } else {
            Ok(())
        }
    }

    fn matches(&self, session: u64, input: CaptureFor) -> bool {
        self.lease
            .as_ref()
            .is_some_and(|lease| lease.key.session == session && lease.key.input == input)
    }
}

pub(super) enum Receipt {
    Windows {
        window: Option<AnyWindowHandle>,
        affinity: platform::Affinity,
    },
    #[cfg(test)]
    Fixture(tests::Prior),
}

#[cfg(test)]
pub(super) use tests::Fake as Fixture;

pub(super) struct Windows<'a> {
    pub(super) native: &'a platform::PresenceNative,
    pub(super) handles: [Option<AnyWindowHandle>; 7],
    pub(super) cx: &'a mut gpui_kit::App,
    #[cfg(test)]
    pub(super) fixture: Option<std::rc::Rc<std::cell::RefCell<Fixture>>>,
}

impl Windows<'_> {
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
                .map(|prior| {
                    prior.map(|mut prior| {
                        prior.window = window;
                        Receipt::Fixture(prior)
                    })
                });
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
            Receipt::Fixture(prior) => {
                self.with_fixture(prior.window, |fixture| fixture.exclude(slot, prior))
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
            Receipt::Fixture(prior) => {
                self.with_fixture(prior.window, |fixture| fixture.restore(slot, prior))
            }
        }
    }

    fn gone(receipt: &Receipt) -> bool {
        match receipt {
            Receipt::Windows { affinity, .. } => affinity.gone(),
            #[cfg(test)]
            Receipt::Fixture(prior) => Fixture::gone(prior),
        }
    }

    fn same(&self, slot: Slot, receipt: &Receipt) -> bool {
        match receipt {
            Receipt::Windows { window, .. } => self.handles[slot as usize] == *window,
            #[cfg(test)]
            Receipt::Fixture(prior) => {
                self.handles[slot as usize] == prior.window
                    && self
                        .fixture
                        .as_ref()
                        .is_some_and(|fixture| fixture.borrow().same(slot, prior))
            }
        }
    }
}

impl Presence {
    #[cfg(test)]
    pub(super) fn check_exclusion_before_show(&self, slot: Slot) {
        if let (Some(lease), Some(fixture)) = (&self.exclusion.lease, &self.exclusion_fixture) {
            assert!(!lease.revoked, "a revoked owner cannot reach placement");
            fixture.borrow().assert_excluded(slot);
        }
    }

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

    pub(super) fn exclusion_handles(&self) -> [Option<AnyWindowHandle>; 7] {
        [
            Some(self.dock_window),
            None,
            Some(self.panel_window),
            self.text.as_ref().map(|panel| panel.window),
            self.thread_panel.as_ref().map(|panel| panel.window),
            self.settings_panel.as_ref().map(|panel| panel.window),
            self.overlay.window().map(|overlay| overlay.window),
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
