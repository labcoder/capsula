#[derive(Clone, Copy)]
pub(super) enum Phase {
    App = 1,
    LyonFill,
    Gpui,
    GpuiShape,
}

#[must_use]
pub(super) struct Guard(#[cfg(test)] u8, #[cfg(test)] u8);

pub(super) fn enter(phase: Phase) -> Guard {
    #[cfg(test)]
    {
        let tag = match phase {
            Phase::App => 1,
            Phase::LyonFill => 2,
            Phase::Gpui => 3,
            Phase::GpuiShape => 4,
        };
        crate::allocation::checkpoint(2, tag);
        Guard(crate::allocation::set_tag(tag), tag)
    }
    #[cfg(not(test))]
    {
        let _ = phase;
        Guard()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        #[cfg(test)]
        {
            crate::allocation::checkpoint(3, self.1);
            crate::allocation::set_tag(self.0);
        }
    }
}

pub(super) fn handoff(path: &gpui_kit::Path<gpui_kit::Pixels>) {
    #[cfg(test)]
    crate::allocation::handoff(
        path.vertices.as_ptr().addr(),
        path.vertices.len() * size_of::<gpui_kit::PathVertex<gpui_kit::Pixels>>(),
        align_of::<gpui_kit::PathVertex<gpui_kit::Pixels>>(),
    );
    #[cfg(not(test))]
    let _ = path;
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(crate) fn assert_paths(events: &[crate::allocation::Request], free_tag: u8) -> usize {
        let mut fills = std::collections::BTreeMap::new();
        let mut filling = false;
        let mut allocations = 0;
        for (index, event) in events.iter().enumerate() {
            match (event.action, event.tag) {
                (2, 2) => {
                    assert!(!filling);
                    filling = true;
                }
                (0, 2) => {
                    assert!(filling);
                    assert_eq!(fills.insert(event.pointer, (event.size, event.align)), None);
                }
                (1, 2) => {
                    assert!(filling);
                    if let Some(layout) = fills.remove(&event.pointer) {
                        assert_eq!(layout, (event.size, event.align));
                    }
                }
                (3, 2) => {
                    assert!(filling);
                    assert!(
                        fills.values().map(|(bytes, _)| bytes).sum::<usize>()
                            <= super::super::paint_geometry::FILL_SCRATCH_BYTES,
                        "fill scratch growth stays inside its fixed allowance"
                    );
                    filling = false;
                }
                (0, 1) => {
                    allocations += 1;
                    let following = &events[index + 1..];
                    let end = following
                        .iter()
                        .position(|next| next.action == 1 && next.pointer == event.pointer)
                        .expect("the application handoff is freed in this interval");
                    assert!(
                        !following[..end]
                            .iter()
                            .any(|next| next.action == 0 && next.pointer == event.pointer),
                        "a pointer cannot be reused before its lifetime ends"
                    );
                    assert_eq!(
                        (
                            following[end].size,
                            following[end].align,
                            following[end].tag
                        ),
                        (event.size, event.align, free_tag)
                    );
                    let handoffs: Vec<_> = following[..end]
                        .iter()
                        .filter(|next| next.action == 4 && next.pointer == event.pointer)
                        .collect();
                    assert_eq!(
                        handoffs.len(),
                        1,
                        "each App allocation is exactly one actual handoff"
                    );
                    assert_eq!(
                        (handoffs[0].size, handoffs[0].align, handoffs[0].tag),
                        (event.size, event.align, 1)
                    );
                }
                _ => {}
            }
        }
        assert!(!filling);
        assert_eq!(
            allocations,
            events.iter().filter(|event| event.action == 4).count(),
            "no unrecorded or extra application handoff"
        );
        allocations
    }

    #[test]
    fn the_path_oracle_checks_each_reused_pointer_lifetime_and_rejects_a_missing_handoff() {
        use crate::allocation::Request;
        let allocation = Request {
            action: 0,
            pointer: 7,
            size: 96,
            align: 4,
            tag: 1,
        };
        let lifetime = [
            allocation,
            Request {
                action: 4,
                ..allocation
            },
            Request {
                action: 1,
                ..allocation
            },
        ];
        let events = [lifetime, lifetime].concat();
        assert_eq!(assert_paths(&events, 1), 2);
        let mut missing = events;
        missing.remove(1);
        assert!(std::panic::catch_unwind(|| assert_paths(&missing, 1)).is_err());
        let fill = [
            Request {
                action: 2,
                pointer: 0,
                size: 0,
                align: 0,
                tag: 2,
            },
            Request {
                pointer: 9,
                size: super::super::paint_geometry::FILL_SCRATCH_BYTES + 1,
                tag: 2,
                ..allocation
            },
            Request {
                action: 3,
                pointer: 0,
                size: 0,
                align: 0,
                tag: 2,
            },
        ];
        assert!(std::panic::catch_unwind(|| assert_paths(&fill, 1)).is_err());
    }

    #[test]
    fn phase_guards_restore_the_outer_phase_even_when_unwinding() {
        let before = crate::allocation::set_tag(7);
        let failed = std::panic::catch_unwind(|| {
            let _app = enter(Phase::App);
            let _fill = enter(Phase::LyonFill);
            panic!("phase guard control");
        });
        let restored = crate::allocation::set_tag(before);
        assert!(failed.is_err());
        assert_eq!(restored, 7);
    }
}
