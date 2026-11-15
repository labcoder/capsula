use super::{ControlToBrain, GestureId, LinkHandle, PoisonError, Sender, Shared};

#[derive(Debug)]
pub(super) enum Stop {
    Awaiting(GestureId),
    Received {
        gesture: GestureId,
        sent_on: Option<u64>,
    },
}

impl Stop {
    fn queue(&mut self, control: &Sender<ControlToBrain>, generation: u64) -> bool {
        let Self::Received { gesture, sent_on } = self else {
            return true;
        };
        if *sent_on == Some(generation) {
            return true;
        }
        if control
            .try_send(ControlToBrain::VoiceHoldStopped { gesture: *gesture })
            .is_err()
        {
            return false;
        }
        *sent_on = Some(generation);
        true
    }
}

impl Shared {
    pub(super) fn queue_voice_control(
        &self,
        out: &Sender<ControlToBrain>,
        message: ControlToBrain,
    ) -> bool {
        let (ControlToBrain::VoiceHoldRecognized { gesture, .. }
        | ControlToBrain::HoldRecognized { gesture, .. }) = &message
        else {
            return out.try_send(message).is_ok();
        };
        let gesture = *gesture;
        let mut pending = self
            .microphone_stop
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if pending.is_some() || out.try_send(message).is_err() {
            return false;
        }
        *pending = Some(Stop::Awaiting(gesture));
        true
    }

    pub(super) fn replay_microphone_stop(&self) {
        let outbox = self.outbox.lock().unwrap_or_else(PoisonError::into_inner);
        let mut pending = self
            .microphone_stop
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(outbox) = outbox.as_ref()
            && let Some(stop) = pending.as_mut()
            && !stop.queue(&outbox.control, outbox.generation)
        {
            outbox.pipe.abort();
        }
    }

    pub(super) fn accept_microphone_stop(&self, generation: u64, gesture: GestureId) {
        let mut pending = self
            .microphone_stop
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if matches!(*pending, Some(Stop::Received { gesture: current, sent_on: Some(sent) })
            if current == gesture && sent == generation)
        {
            *pending = None;
        }
    }
}

impl LinkHandle {
    #[cfg(any(test, feature = "test-support"))]
    pub fn accept_recorded_microphone_stop(&self, generation: u64, gesture: GestureId) {
        assert!(
            self.control_recording.is_some(),
            "only a recording link accepts a recorded stop"
        );
        self.shared.accept_microphone_stop(generation, gesture);
    }

    pub fn voice_stop_pending(&self) -> bool {
        self.shared
            .microphone_stop
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    pub(super) fn retain_microphone_stop(&self, gesture: GestureId) -> Option<bool> {
        let outbox = self
            .shared
            .outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut pending = self
            .shared
            .microphone_stop
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let stop = pending.as_mut()?;
        let current = match stop {
            Stop::Awaiting(current)
            | Stop::Received {
                gesture: current, ..
            } => *current,
        };
        if current != gesture {
            return None;
        }
        if matches!(stop, Stop::Awaiting(_)) {
            *stop = Stop::Received {
                gesture,
                sent_on: None,
            };
        }
        #[cfg(any(test, feature = "test-support"))]
        if let Some(recording) = &self.control_recording {
            return Some(stop.queue(recording, self.link_generation()));
        }
        if let Some(outbox) = outbox.as_ref()
            && !stop.queue(&outbox.control, outbox.generation)
        {
            outbox.pipe.abort();
        }
        Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microphone_stop_reservation_survives_neither_full_nor_closed_request_refusal() {
        let shared = Shared::default();
        let (out, queue) = async_channel::bounded(1);
        let recognized = ControlToBrain::VoiceHoldRecognized {
            gesture: GestureId(7),
            thread: None,
            selection: 0,
        };
        out.try_send(ControlToBrain::Pong).unwrap();
        assert!(!shared.queue_voice_control(&out, recognized.clone()));
        assert!(shared.microphone_stop.lock().unwrap().is_none());
        assert_eq!(queue.try_recv(), Ok(ControlToBrain::Pong));
        out.close();
        assert!(!shared.queue_voice_control(&out, recognized.clone()));
        assert!(shared.microphone_stop.lock().unwrap().is_none());
        let (out, queue) = async_channel::bounded(1);
        assert!(shared.queue_voice_control(&out, recognized.clone()));
        assert_eq!(queue.try_recv(), Ok(recognized));
        assert!(matches!(
            *shared.microphone_stop.lock().unwrap(),
            Some(Stop::Awaiting(GestureId(7)))
        ));
    }

    #[test]
    fn a_screen_hold_reserves_the_microphone_stop_as_a_voice_hold_does() {
        let shared = Shared::default();
        let (out, queue) = async_channel::bounded(2);
        let recognized = ControlToBrain::HoldRecognized {
            gesture: GestureId(7),
            thread: None,
            selection: 0,
            pointer: kumochi_protocol::geometry::PhysicalPoint([0; 2]),
            lease: kumochi_protocol::message::LeaseGeneration(1),
            t_qpc: 0,
        };
        assert!(shared.queue_voice_control(&out, recognized.clone()));
        assert_eq!(queue.try_recv(), Ok(recognized.clone()));
        assert!(matches!(
            *shared.microphone_stop.lock().unwrap(),
            Some(Stop::Awaiting(GestureId(7)))
        ));
        assert!(!shared.queue_voice_control(&out, recognized));
        assert_eq!(queue.try_recv(), Err(async_channel::TryRecvError::Empty));
    }

    #[test]
    fn microphone_stop_queue_failure_keeps_the_receipt_for_the_next_connection() {
        let mut stop = Stop::Awaiting(GestureId(7));
        let (out, queue) = async_channel::bounded(1);
        assert!(stop.queue(&out, 1));
        assert_eq!(queue.try_recv(), Err(async_channel::TryRecvError::Empty));
        stop = Stop::Received {
            gesture: GestureId(7),
            sent_on: None,
        };
        out.try_send(ControlToBrain::Pong).unwrap();
        assert!(!stop.queue(&out, 1));
        assert_eq!(queue.try_recv(), Ok(ControlToBrain::Pong));
        assert!(stop.queue(&out, 1));
        assert!(stop.queue(&out, 1));
        assert_eq!(
            queue.try_recv(),
            Ok(ControlToBrain::VoiceHoldStopped {
                gesture: GestureId(7)
            })
        );
        assert_eq!(queue.try_recv(), Err(async_channel::TryRecvError::Empty));
        out.close();
        assert!(!stop.queue(&out, 2));
        let (out, queue) = async_channel::bounded(1);
        assert!(stop.queue(&out, 2));
        assert_eq!(
            queue.try_recv(),
            Ok(ControlToBrain::VoiceHoldStopped {
                gesture: GestureId(7)
            })
        );
        assert_eq!(queue.try_recv(), Err(async_channel::TryRecvError::Empty));
    }
}
