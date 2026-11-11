use std::{cell::Cell, marker::PhantomData, rc::Rc};

use super::{RECORD, RECORD_LOST, Record, Request, TAG};

pub(crate) const CAPACITY: usize = 16_384;

thread_local! {
    static OWNER: Cell<Option<usize>> = const { Cell::new(None) };
}

pub(crate) struct PreparedRequests(Option<Record>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScopeFailure {
    Unavailable,
    Borrowed,
    Active,
    NotOwner,
}

pub(crate) struct InstallRefused {
    pub(crate) reason: ScopeFailure,
    pub(crate) prepared: PreparedRequests,
}

pub(crate) struct LocalRequests {
    pointer: usize,
    tag: u8,
    lost: bool,
    active: bool,
    thread: PhantomData<Rc<()>>,
}

pub(crate) struct FinishedRequests {
    pub(crate) requests: Vec<Request>,
    pub(crate) overflow: bool,
    pub(crate) lost: bool,
}

impl PreparedRequests {
    pub(crate) fn new() -> Self {
        Self(Some(Record {
            requests: Vec::with_capacity(CAPACITY),
            overflow: false,
        }))
    }

    pub(crate) fn storage(&self) -> Option<(usize, usize, usize)> {
        self.0.as_ref().map(|record| {
            (
                record.requests.as_ptr().addr(),
                record.requests.capacity() * size_of::<Request>(),
                align_of::<Request>(),
            )
        })
    }

    pub(crate) fn install(mut self) -> Result<LocalRequests, InstallRefused> {
        let mut reason = ScopeFailure::Unavailable;
        let installed = RECORD
            .try_with(|record| {
                let Ok(mut slot) = record.try_borrow_mut() else {
                    reason = ScopeFailure::Borrowed;
                    return None;
                };
                if slot.is_some() {
                    reason = ScopeFailure::Active;
                    return None;
                }
                TAG.try_with(|tag| {
                    RECORD_LOST
                        .try_with(|lost| {
                            OWNER
                                .try_with(|owner| {
                                    if owner.get().is_some() {
                                        reason = ScopeFailure::Active;
                                        return None;
                                    }
                                    let prepared = self.0.take()?;
                                    let pointer = prepared.requests.as_ptr().addr();
                                    let local = LocalRequests {
                                        pointer,
                                        tag: tag.get(),
                                        lost: lost.get(),
                                        active: true,
                                        thread: PhantomData,
                                    };
                                    *slot = Some(prepared);
                                    owner.set(Some(pointer));
                                    tag.set(1);
                                    lost.set(false);
                                    Some(local)
                                })
                                .ok()
                                .flatten()
                        })
                        .ok()
                        .flatten()
                })
                .ok()
                .flatten()
            })
            .ok()
            .flatten();
        installed.ok_or(InstallRefused {
            reason,
            prepared: self,
        })
    }
}

impl LocalRequests {
    fn detach(&mut self) -> Result<Record, ScopeFailure> {
        if !self.active {
            return Err(ScopeFailure::NotOwner);
        }
        let record = RECORD
            .try_with(|record| {
                let mut slot = record
                    .try_borrow_mut()
                    .map_err(|_| ScopeFailure::Borrowed)?;
                OWNER
                    .try_with(|owner| {
                        if owner.get() != Some(self.pointer)
                            || slot.as_ref().map(|r| r.requests.as_ptr().addr())
                                != Some(self.pointer)
                        {
                            return Err(ScopeFailure::NotOwner);
                        }
                        let taken = slot.take().ok_or(ScopeFailure::NotOwner)?;
                        owner.set(None);
                        Ok(taken)
                    })
                    .unwrap_or(Err(ScopeFailure::Unavailable))
            })
            .unwrap_or(Err(ScopeFailure::Unavailable))?;
        self.active = false;
        Ok(record)
    }

    pub(crate) fn finish(&mut self) -> Result<FinishedRequests, ScopeFailure> {
        let record = self.detach()?;
        let lost = RECORD_LOST.try_with(Cell::get);
        let restored_tag = TAG.try_with(|tag| tag.set(self.tag));
        let restored_lost = RECORD_LOST.try_with(|lost| lost.set(self.lost));
        if restored_tag.is_err() || restored_lost.is_err() {
            return Err(ScopeFailure::Unavailable);
        }
        Ok(FinishedRequests {
            requests: record.requests,
            overflow: record.overflow,
            lost: lost.map_err(|_| ScopeFailure::Unavailable)?,
        })
    }
}

impl Drop for LocalRequests {
    fn drop(&mut self) {
        if let Ok(record) = self.detach() {
            let _tag = TAG.try_with(|tag| tag.set(self.tag));
            let _lost = RECORD_LOST.try_with(|lost| lost.set(self.lost));
            drop(record);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(prepared: PreparedRequests) -> LocalRequests {
        match prepared.install() {
            Ok(local) => local,
            Err(_) => panic!("a vacant recorder accepts prepared storage"),
        }
    }

    #[test]
    fn prepared_scopes_refuse_borrows_and_active_records_without_changing_them() {
        let prepared = PreparedRequests::new();
        let storage = prepared.storage();
        let tag = TAG.with(|value| value.replace(7));
        let lost = RECORD_LOST.with(|value| value.replace(true));
        let refused = RECORD.with(|record| {
            let borrow = record.borrow_mut();
            let Err(refused) = prepared.install() else {
                panic!("borrowed installation must refuse");
            };
            assert_eq!(refused.reason, ScopeFailure::Borrowed);
            assert_eq!(refused.prepared.storage(), storage);
            assert_eq!(TAG.with(Cell::get), 7);
            assert!(RECORD_LOST.with(Cell::get));
            drop(borrow);
            refused.prepared
        });
        let unused = PreparedRequests::new();
        let mut local = installed(refused);
        let refused = RECORD.with(|record| {
            let borrow = record.borrow_mut();
            let Err(refused) = unused.install() else {
                panic!("borrowed active installation must refuse");
            };
            assert_eq!(refused.reason, ScopeFailure::Borrowed);
            assert!(!RECORD_LOST.with(Cell::get));
            assert_eq!(TAG.with(Cell::get), 1);
            drop(borrow);
            refused.prepared
        });
        let Err(refused) = refused.install() else {
            panic!("active installation must refuse");
        };
        assert_eq!(refused.reason, ScopeFailure::Active);
        assert_eq!(TAG.with(Cell::get), 1);
        assert!(!RECORD_LOST.with(Cell::get));
        let (_, counts, bytes) = super::super::usage(|| {
            drop(std::hint::black_box(vec![0_u8; 73].into_boxed_slice()));
        });
        let finished = local.finish().expect("the installed scope detaches");
        assert_eq!((counts, bytes), ([1, 1], [73, 73]));
        assert_eq!(finished.requests.len(), 2);
        assert_eq!(
            (finished.requests[0].action, finished.requests[0].size),
            (0, 73)
        );
        assert_eq!(
            (finished.requests[1].action, finished.requests[1].size),
            (1, 73)
        );
        assert_eq!(finished.requests[0].pointer, finished.requests[1].pointer);
        assert!(!finished.overflow);
        assert!(!finished.lost);
        assert_eq!(TAG.with(Cell::get), 7);
        assert!(RECORD_LOST.with(Cell::get));
        TAG.with(|value| value.set(tag));
        RECORD_LOST.with(|value| value.set(lost));
        drop(refused);
    }

    #[test]
    fn a_borrowed_finish_can_be_retried_without_detaching_the_owner() {
        let mut local = installed(PreparedRequests::new());
        RECORD.with(|record| {
            let borrow = record.borrow_mut();
            assert!(matches!(local.finish(), Err(ScopeFailure::Borrowed)));
            assert_eq!(OWNER.with(Cell::get), Some(local.pointer));
            assert_eq!(TAG.with(Cell::get), 1);
            drop(borrow);
        });
        OWNER.with(|owner| owner.set(None));
        assert!(matches!(local.finish(), Err(ScopeFailure::NotOwner)));
        assert!(RECORD.with(|record| record.borrow().is_some()));
        OWNER.with(|owner| owner.set(Some(local.pointer)));
        let finished = local.finish().expect("the restored owner can detach");
        assert_eq!(finished.requests.len(), 0);
        assert!(!finished.overflow);
        assert!(!finished.lost);
        assert!(matches!(local.finish(), Err(ScopeFailure::NotOwner)));
    }

    #[test]
    fn prepared_scopes_report_overflow_and_reentry_after_normal_callback_return() {
        let mut local = installed(PreparedRequests::new());
        for _ in 0..=CAPACITY {
            super::super::checkpoint(6, 1);
        }
        let finished = local
            .finish()
            .expect("overflow is returned outside allocation");
        assert_eq!(finished.requests.len(), CAPACITY);
        assert!(finished.overflow);
        assert!(!finished.lost);
        let mut local = installed(PreparedRequests::new());
        let value = RECORD.with(|record| {
            let borrow = record.borrow_mut();
            let value = std::hint::black_box(vec![0_u8; 123].into_boxed_slice());
            drop(borrow);
            value
        });
        drop(value);
        let finished = local
            .finish()
            .expect("reentry is returned outside allocation");
        assert_eq!(finished.requests.len(), 1);
        assert_eq!(
            (finished.requests[0].action, finished.requests[0].size),
            (1, 123)
        );
        assert!(finished.lost);
        assert!(!finished.overflow);
    }

    #[test]
    fn an_unwinding_local_scope_restores_tls_before_releasing_storage() {
        let prepared = PreparedRequests::new();
        let tag = TAG.with(|value| value.replace(9));
        let lost = RECORD_LOST.with(|value| value.replace(true));
        let entered = Cell::new(false);
        let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _local = installed(prepared);
            entered.set(true);
            panic!("local scope unwind control");
        }));
        assert!(entered.get());
        let failure = failure.expect_err("the installed scope's body unwinds");
        assert_eq!(
            failure.downcast_ref::<&str>().copied(),
            Some("local scope unwind control")
        );
        assert!(RECORD.with(|record| record.borrow().is_none()));
        assert_eq!(OWNER.with(Cell::get), None);
        assert_eq!(TAG.with(Cell::get), 9);
        assert!(RECORD_LOST.with(Cell::get));
        TAG.with(|value| value.set(tag));
        RECORD_LOST.with(|value| value.set(lost));
        let ((), events) = super::super::requests(2, || {
            drop(std::hint::black_box(vec![0_u8; 17]));
        });
        assert_eq!(events.len(), 2);
        assert_eq!((events[0].action, events[0].size), (0, 17));
        assert_eq!((events[1].action, events[1].size), (1, 17));
    }

    #[test]
    fn a_borrowed_drop_does_not_panic_or_claim_restoration() {
        let local = installed(PreparedRequests::new());
        let (pointer, tag, lost) = (local.pointer, local.tag, local.lost);
        RECORD.with(|record| {
            let borrow = record.borrow_mut();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(local)));
            assert!(result.is_ok());
            assert_eq!(OWNER.with(Cell::get), Some(pointer));
            assert_eq!(TAG.with(Cell::get), 1);
            assert!(!RECORD_LOST.with(Cell::get));
            drop(borrow);
        });
        let record = RECORD.with(|record| record.borrow_mut().take());
        OWNER.with(|owner| owner.set(None));
        TAG.with(|value| value.set(tag));
        RECORD_LOST.with(|value| value.set(lost));
        drop(record);
    }
}
