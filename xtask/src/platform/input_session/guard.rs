//! Bounded, content-free first-stop metadata shared through numeric window properties.

use std::cell::Cell;
use std::time::Instant;

use anyhow::{Result, ensure};
use capsula_protocol::constants::FOCUS_INPUT_MARKER;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, MOUSE_MOVE_RELATIVE, RAWINPUT, RAWINPUTHEADER, RID_INPUT,
    RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{RI_MOUSE_HWHEEL, RI_MOUSE_WHEEL, WM_APP};
use windows::core::{PCWSTR, w};

use super::fixture::{ABORT, property, set_property};
use super::{MARKER, pack, size, unpack};

pub(super) const SET_PHASE: u32 = WM_APP + 24;
const CAUSE: PCWSTR = w!("InputSessionStopCause");
const PHASE: PCWSTR = w!("InputSessionStopPhase");
const ELAPSED: PCWSTR = w!("InputSessionStopElapsed");
const MOTION: PCWSTR = w!("InputSessionStopMotion");
const MARK: PCWSTR = w!("InputSessionStopMarker");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopCause {
    Lock,
    Input(PhysicalInput),
    GuardReadFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputClass {
    Keyboard,
    MouseMove { dx: i32, dy: i32 },
    Button,
    Wheel,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PhysicalInput {
    pub(crate) class: InputClass,
    pub(crate) marker: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Observation {
    pub(super) input: PhysicalInput,
    pub(super) exempt: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StopMetadata {
    pub(crate) cause: StopCause,
    pub(crate) phase: u8,
    /// Milliseconds since the native injection gate opened; absent before opening.
    pub(crate) elapsed_ms: Option<u64>,
}

#[derive(Default)]
struct Guard {
    phase: u8,
    opened: Option<Instant>,
    first: Option<StopMetadata>,
}

impl Guard {
    fn stop(&mut self, cause: StopCause, now: Instant) -> StopMetadata {
        *self.first.get_or_insert_with(|| StopMetadata {
            cause,
            phase: self.phase,
            elapsed_ms: self.opened.map(|opened| {
                u64::try_from(now.saturating_duration_since(opened).as_millis())
                    .unwrap_or(u64::MAX - 1)
            }),
        })
    }
}

thread_local! {
    static GUARD: Cell<Guard> = Cell::new(Guard::default());
}

pub(super) fn set_phase(phase: u8) {
    GUARD.with(|slot| {
        let mut guard = slot.take();
        guard.phase = phase;
        slot.set(guard);
    });
}

pub(super) fn opened() {
    GUARD.with(|slot| {
        let mut guard = slot.take();
        guard.opened.get_or_insert_with(Instant::now);
        slot.set(guard);
    });
}

pub(super) fn stop(root: HWND, cause: StopCause) -> Result<()> {
    set_property(root, ABORT, 1)?;
    let first = GUARD.with(|slot| {
        let mut guard = slot.take();
        let first = guard.stop(cause, Instant::now());
        slot.set(guard);
        first
    });
    if property(root, CAUSE) == 0 {
        let (code, motion, marker) = encode(first.cause);
        set_property(root, PHASE, usize::from(first.phase))?;
        set_property(
            root,
            ELAPSED,
            usize::try_from(first.elapsed_ms.map_or(0, |ms| ms.saturating_add(1)))?,
        )?;
        set_property(root, MOTION, usize::try_from(motion)?)?;
        set_property(
            root,
            MARK,
            marker.map_or(0, |marker| {
                usize::try_from(u64::from(marker) + 1)
                    .expect("a raw-input marker fits a 64-bit property")
            }),
        )?;
        set_property(root, CAUSE, code)?;
    }
    Ok(())
}

fn encode(cause: StopCause) -> (usize, u64, Option<u32>) {
    match cause {
        StopCause::Lock => (1, 0, None),
        StopCause::GuardReadFailure => (2, 0, None),
        StopCause::Input(input) => {
            let (code, motion) = match input.class {
                InputClass::Keyboard => (3, 0),
                InputClass::MouseMove { dx, dy } => (4, pack(dx, dy)),
                InputClass::Button => (5, 0),
                InputClass::Wheel => (6, 0),
                InputClass::Other => (7, 0),
            };
            (code, motion, input.marker)
        }
    }
}

pub(super) fn snapshot(root: HWND) -> Option<StopMetadata> {
    let code = property(root, CAUSE);
    let cause = match code {
        1 => StopCause::Lock,
        2 => StopCause::GuardReadFailure,
        3..=7 => {
            let class = match code {
                3 => InputClass::Keyboard,
                4 => {
                    let [dx, dy] = unpack(u64::try_from(property(root, MOTION)).ok()?);
                    InputClass::MouseMove { dx, dy }
                }
                5 => InputClass::Button,
                6 => InputClass::Wheel,
                _ => InputClass::Other,
            };
            StopCause::Input(PhysicalInput {
                class,
                marker: property(root, MARK)
                    .checked_sub(1)
                    .map(u32::try_from)
                    .transpose()
                    .ok()?,
            })
        }
        _ => return None,
    };
    Some(StopMetadata {
        cause,
        phase: u8::try_from(property(root, PHASE)).ok()?,
        elapsed_ms: u64::try_from(property(root, ELAPSED)).ok()?.checked_sub(1),
    })
}

pub(super) fn physical_input(param: LPARAM) -> Result<Option<PhysicalInput>> {
    let observation = read_input(param)?;
    Ok((!observation.exempt).then_some(observation.input))
}

pub(super) fn read_input(param: LPARAM) -> Result<Observation> {
    let mut raw = RAWINPUT::default();
    let mut bytes = size::<RAWINPUT>();
    // SAFETY: RAWINPUT is aligned and writable for its stated size; the message owns this raw-input handle.
    let read = unsafe {
        GetRawInputData(
            HRAWINPUT(std::ptr::with_exposed_provenance_mut(
                param.0.cast_unsigned(),
            )),
            RID_INPUT,
            Some((&raw mut raw).cast()),
            &raw mut bytes,
            size::<RAWINPUTHEADER>(),
        )
    };
    ensure!(
        read != u32::MAX && read >= size::<RAWINPUTHEADER>() && bytes <= size::<RAWINPUT>(),
        "raw-input guard could not read an event"
    );
    let required = match raw.header.dwType {
        value if value == RIM_TYPEKEYBOARD.0 => size_of::<windows::Win32::UI::Input::RAWKEYBOARD>(),
        value if value == RIM_TYPEMOUSE.0 => size_of::<windows::Win32::UI::Input::RAWMOUSE>(),
        _ => 0,
    };
    ensure!(
        usize::try_from(read)? >= size_of::<RAWINPUTHEADER>() + required,
        "raw-input guard read a truncated event"
    );
    classify(&raw)
}

#[cfg(test)]
pub(super) fn physical_event(raw: &RAWINPUT) -> Result<Option<PhysicalInput>> {
    let observation = classify(raw)?;
    Ok((!observation.exempt).then_some(observation.input))
}

fn classify(raw: &RAWINPUT) -> Result<Observation> {
    let (class, marker, focus_motion) = match raw.header.dwType {
        value if value == RIM_TYPEKEYBOARD.0 => {
            // SAFETY: the header identifies the keyboard union member; no key identity is retained.
            (
                InputClass::Keyboard,
                Some(unsafe { raw.data.keyboard }.ExtraInformation),
                false,
            )
        }
        value if value == RIM_TYPEMOUSE.0 => {
            // SAFETY: the header identifies the mouse union member.
            let mouse = unsafe { raw.data.mouse };
            let marker = mouse.ulExtraInformation;
            // SAFETY: both union views describe the same initialized button flags/data bytes.
            let buttons = unsafe { mouse.Anonymous.ulButtons };
            let focus_motion = usize::try_from(marker)? == FOCUS_INPUT_MARKER
                && mouse.usFlags == MOUSE_MOVE_RELATIVE
                && mouse.lLastX == 0
                && mouse.lLastY == 0
                && buttons == 0
                && mouse.ulRawButtons == 0;
            let class = if buttons & (RI_MOUSE_WHEEL | RI_MOUSE_HWHEEL) != 0 {
                InputClass::Wheel
            } else if buttons != 0 || mouse.ulRawButtons != 0 {
                InputClass::Button
            } else if mouse.lLastX != 0 || mouse.lLastY != 0 {
                InputClass::MouseMove {
                    dx: mouse.lLastX,
                    dy: mouse.lLastY,
                }
            } else {
                InputClass::Other
            };
            (class, Some(marker), focus_motion)
        }
        _ => (InputClass::Other, None, false),
    };
    Ok(Observation {
        input: PhysicalInput { class, marker },
        exempt: focus_motion || marker.map(usize::try_from).transpose()? == Some(MARKER),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use windows::Win32::UI::Input::{RAWKEYBOARD, RAWMOUSE, RAWMOUSE_0, RIM_TYPEHID};

    fn mouse(mouse: RAWMOUSE) -> PhysicalInput {
        let mut raw = RAWINPUT::default();
        raw.header.dwType = RIM_TYPEMOUSE.0;
        raw.data.mouse = mouse;
        physical_event(&raw).unwrap().unwrap()
    }

    #[test]
    fn classification_keeps_signed_motion_and_marker_but_no_key_identity() {
        assert_eq!(
            mouse(RAWMOUSE {
                lLastX: -71,
                lLastY: 29,
                ulExtraInformation: 123,
                ..Default::default()
            }),
            PhysicalInput {
                class: InputClass::MouseMove { dx: -71, dy: 29 },
                marker: Some(123)
            }
        );
        for (buttons, class) in [
            (1, InputClass::Button),
            (0x400, InputClass::Wheel),
            (0x800, InputClass::Wheel),
        ] {
            assert_eq!(
                mouse(RAWMOUSE {
                    Anonymous: RAWMOUSE_0 { ulButtons: buttons },
                    ulExtraInformation: 9,
                    ..Default::default()
                }),
                PhysicalInput {
                    class,
                    marker: Some(9)
                }
            );
        }
        assert_eq!(
            mouse(RAWMOUSE::default()),
            PhysicalInput {
                class: InputClass::Other,
                marker: Some(0)
            }
        );
        let mut raw = RAWINPUT::default();
        raw.header.dwType = RIM_TYPEKEYBOARD.0;
        for code in [0, 12, u16::MAX] {
            raw.data.keyboard = RAWKEYBOARD {
                MakeCode: code,
                VKey: code,
                ExtraInformation: 43,
                ..Default::default()
            };
            assert_eq!(
                physical_event(&raw).unwrap(),
                Some(PhysicalInput {
                    class: InputClass::Keyboard,
                    marker: Some(43)
                })
            );
        }
        raw.header.dwType = RIM_TYPEHID.0;
        assert_eq!(
            physical_event(&raw).unwrap(),
            Some(PhysicalInput {
                class: InputClass::Other,
                marker: None
            })
        );
    }

    #[test]
    fn session_tagged_keyboard_and_mouse_never_stop_the_guard() {
        let mut raw = RAWINPUT::default();
        raw.header.dwType = RIM_TYPEKEYBOARD.0;
        raw.data.keyboard = RAWKEYBOARD {
            ExtraInformation: u32::try_from(MARKER).unwrap(),
            ..Default::default()
        };
        assert_eq!(physical_event(&raw).unwrap(), None);
        assert_eq!(
            classify(&raw).unwrap(),
            Observation {
                input: PhysicalInput {
                    class: InputClass::Keyboard,
                    marker: Some(u32::try_from(MARKER).unwrap())
                },
                exempt: true,
            }
        );
        raw.header.dwType = RIM_TYPEMOUSE.0;
        raw.data.mouse = RAWMOUSE {
            lLastX: -5,
            Anonymous: RAWMOUSE_0 { ulButtons: 1 },
            ulExtraInformation: u32::try_from(MARKER).unwrap(),
            ..Default::default()
        };
        assert_eq!(physical_event(&raw).unwrap(), None);
        assert_eq!(
            classify(&raw).unwrap(),
            Observation {
                input: PhysicalInput {
                    class: InputClass::Button,
                    marker: Some(u32::try_from(MARKER).unwrap())
                },
                exempt: true,
            }
        );
        raw.data.mouse = RAWMOUSE {
            ulExtraInformation: u32::try_from(FOCUS_INPUT_MARKER).unwrap(),
            ..Default::default()
        };
        assert_eq!(
            classify(&raw).unwrap(),
            Observation {
                input: PhysicalInput {
                    class: InputClass::Other,
                    marker: Some(u32::try_from(FOCUS_INPUT_MARKER).unwrap())
                },
                exempt: true,
            }
        );
    }

    #[test]
    fn first_cause_phase_and_native_elapsed_are_immutable_for_every_stop_order() {
        let now = Instant::now();
        let causes = [
            StopCause::Lock,
            StopCause::GuardReadFailure,
            StopCause::Input(PhysicalInput {
                class: InputClass::Keyboard,
                marker: Some(7),
            }),
        ];
        for first in causes {
            let mut guard = Guard {
                phase: 3,
                opened: Some(now),
                first: None,
            };
            let expected = StopMetadata {
                cause: first,
                phase: 3,
                elapsed_ms: Some(17),
            };
            assert_eq!(guard.stop(first, now + Duration::from_millis(17)), expected);
            guard.phase = 8;
            for later in causes {
                assert_eq!(guard.stop(later, now + Duration::from_secs(2)), expected);
            }
        }
        let mut guard = Guard::default();
        assert_eq!(guard.stop(StopCause::Lock, now).elapsed_ms, None);
    }
}
