use windows::Win32::Foundation::HANDLE;

use crate::input::{Edge, Event, Key, PhysicalKey, TalkHeld};

const CONTROL: u16 = 0x1D;
const ALT: u16 = 0x38;

#[derive(Default)]
struct Device {
    handle: HANDLE,
    down: [u64; 8],
}

impl Device {
    fn neutral(&self) -> bool {
        self.down == [0; 8]
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
    overflow: Vec<Device>,
    inert: bool,
}

impl Physical {
    pub(super) fn lose(&mut self) -> Event {
        self.wait_for_breaks();
        Event::Reset
    }

    fn wait_for_breaks(&mut self) {
        self.inert = !self.overflow.is_empty() || self.devices.iter().any(|d| !d.neutral());
        if !self.inert {
            self.overflow = Vec::new();
        }
    }

    pub(super) fn remove(&mut self, handle: HANDLE) {
        if let Some(device) = self.devices.iter_mut().find(|d| d.handle == handle) {
            *device = Device::default();
        }
        self.overflow.retain(|device| device.handle != handle);
    }

    pub(super) fn key(
        &mut self,
        handle: HANDLE,
        slot: u16,
        event: Event,
    ) -> Option<(Event, Option<PhysicalKey>, bool)> {
        let reset = |this: &mut Self| Some((this.lose(), None, false));
        let Event::Key { key, edge } = event else {
            return reset(self);
        };
        if handle.is_invalid() {
            return reset(self);
        }
        if self.inert
            && let Some(index) = self.overflow.iter().position(|d| d.handle == handle)
        {
            let device = &mut self.overflow[index];
            device.apply(slot, edge);
            if device.neutral() {
                self.overflow.swap_remove(index);
                self.wait_for_breaks();
            }
            return None;
        }
        let Some(bank) = self
            .devices
            .iter()
            .position(|d| d.handle == handle)
            .or_else(|| self.devices.iter().position(Device::neutral))
        else {
            if edge == Edge::Down {
                let mut device = Device {
                    handle,
                    ..Device::default()
                };
                device.apply(slot, edge);
                self.overflow.push(device);
            }
            return if self.inert { None } else { reset(self) };
        };
        let other_held = self.devices[1 - bank].held(slot);
        let device = &mut self.devices[bank];
        device.handle = handle;
        let repeated = device.apply(slot, edge);
        let modifier = matches!(key, Key::LeftControl | Key::LeftAlt);
        if modifier && other_held && (edge == Edge::Down || !repeated) && !self.inert {
            return reset(self);
        }
        if self.inert {
            self.wait_for_breaks();
            return None;
        }
        let id = identity(bank, slot);
        let key = match key {
            Key::Escape(_) => Key::Escape(id),
            Key::Other(_) => Key::Other(id),
            _ => key,
        };
        Some((
            Event::Key { key, edge },
            modifier.then_some(id),
            modifier && repeated && edge == Edge::Down,
        ))
    }

    pub(super) fn held(&self) -> TalkHeld {
        let down = |slot| {
            self.devices
                .iter()
                .position(|device| device.held(slot))
                .map(|bank| identity(bank, slot))
        };
        TalkHeld([down(CONTROL), down(ALT)])
    }
}

fn identity(bank: usize, slot: u16) -> PhysicalKey {
    let bank = u16::try_from(bank).expect("two device banks");
    PhysicalKey::new(bank * 512 + slot).expect("validated slot in one of two banks")
}
