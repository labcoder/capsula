use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};

use crate::input::{Edge, Event, Key, PhysicalKey, TalkHeld};

const CONTROL: u16 = 0x1D;
const ALT: u16 = 0x38;

#[derive(Default)]
struct Device {
    handle: Option<HANDLE>,
    down: [u64; 8],
    needs_break: [bool; 2],
}

impl Device {
    fn neutral(&self) -> bool {
        self.down == [0; 8] && self.needs_break == [false; 2]
    }

    fn held(&self, slot: u16) -> bool {
        self.down[usize::from(slot / 64)] & (1 << (slot % 64)) != 0
    }

    fn apply(&mut self, slot: u16, edge: Edge) -> bool {
        let bit = 1 << (slot % 64);
        let word = &mut self.down[usize::from(slot / 64)];
        let was_down = *word & bit != 0;
        match edge {
            Edge::Down => *word |= bit,
            Edge::Up => *word &= !bit,
        }
        was_down
    }
}

#[derive(Default)]
pub(super) struct Physical {
    devices: [Device; 2],
}

impl Physical {
    pub(super) fn reset(&mut self) -> Event {
        for device in &mut self.devices {
            for (index, slot) in [CONTROL, ALT].into_iter().enumerate() {
                device.needs_break[index] |= device.held(slot);
            }
        }
        Event::Reset
    }

    pub(super) fn lose(&mut self) -> Event {
        let reset = self.reset();
        for device in &mut self.devices {
            device.down = [0; 8];
        }
        reset
    }

    pub(super) fn device_changed(&mut self, removed: Option<HANDLE>) -> Event {
        if let Some(removed) = removed
            && let Some(device) = self.devices.iter_mut().find(|d| d.handle == Some(removed))
        {
            *device = Device::default();
        }
        self.reset()
    }

    pub(super) fn key(
        &mut self,
        handle: HANDLE,
        slot: u16,
        event: Event,
        queued_press: Option<u64>,
        covered_after: Option<u64>,
    ) -> Option<(Event, Option<PhysicalKey>, bool)> {
        let reset = |this: &mut Self| Some((this.lose(), None, false));
        let Event::Key { key, edge } = event else {
            return reset(self);
        };
        if handle == INVALID_HANDLE_VALUE {
            return reset(self);
        }
        let modifier = match key {
            Key::LeftControl => Some(0),
            Key::LeftAlt => Some(1),
            _ => None,
        };
        let handle = Some(handle);
        let known = self.devices.iter().position(|d| d.handle == handle);
        let free = self
            .devices
            .iter()
            .position(|device| device.handle.is_none());
        if known.or(free).is_none() && modifier.is_none() && edge == Edge::Up {
            return None;
        }
        let Some(bank) = known
            .or(free)
            .or_else(|| self.devices.iter().position(Device::neutral))
        else {
            return reset(self);
        };
        let other_held = self.devices[1 - bank].held(slot);
        let device = &mut self.devices[bank];
        if device.handle != handle {
            *device = Device {
                handle,
                ..Device::default()
            };
        }
        let repeated = device.apply(slot, edge);
        if modifier.is_some() && other_held && (edge == Edge::Down || !repeated) {
            return reset(self);
        }
        if let Some(index) = modifier {
            if edge == Edge::Down
                && covered_after
                    .is_some_and(|boundary| queued_press.is_none_or(|at| at <= boundary))
            {
                device.needs_break[index] = true;
            }
            if edge == Edge::Down && device.needs_break[index] {
                return Some((event, None, true));
            }
            if edge == Edge::Up {
                device.needs_break[index] = false;
            }
        }
        let id = identity(bank, slot);
        let key = match key {
            Key::Escape(_) => Key::Escape(id),
            Key::Other(_) => Key::Other(id),
            _ => key,
        };
        Some((
            Event::Key { key, edge },
            modifier.map(|_| id),
            modifier.is_some() && repeated && edge == Edge::Down,
        ))
    }

    pub(super) fn held(&self) -> TalkHeld {
        let down = |slot| {
            self.devices
                .iter()
                .position(|device| {
                    device.held(slot) && !device.needs_break[usize::from(slot == ALT)]
                })
                .map(|bank| identity(bank, slot))
        };
        TalkHeld([down(CONTROL), down(ALT)])
    }
}

fn identity(bank: usize, slot: u16) -> PhysicalKey {
    let bank = u16::try_from(bank).expect("two device banks");
    PhysicalKey::new(bank * 512 + slot).expect("validated slot in one of two banks")
}
