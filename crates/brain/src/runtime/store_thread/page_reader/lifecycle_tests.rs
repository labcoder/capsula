use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use super::Barrier;
use crate::recall::{Error, control::Control};

#[test]
fn every_barrier_wait_observes_cancel_eof_and_deadline() {
    for cause in 0..3 {
        let (observed, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let barrier = Arc::new(Barrier::new(Some(observed)));
        let control = Control::new(Instant::now() + Duration::from_secs(60));
        let (reply, result) = mpsc::channel();
        let worker = {
            let barrier = Arc::clone(&barrier);
            let control = control.clone();
            std::thread::spawn(move || reply.send(barrier.wait_until(1, &control)).expect("reply"))
        };
        let limit = Instant::now() + Duration::from_secs(10);
        let observed = loop {
            if let Ok(seen) = seen.try_recv() {
                break seen;
            }
            assert!(Instant::now() < limit, "barrier wait observed");
            std::thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(observed, crate::runtime::Seen::ReadWaiting);
        match cause {
            0 => control.cancel(),
            1 => barrier.end(),
            2 => *control.test.time.lock().expect("clock") = Some(control.end),
            _ => unreachable!(),
        }
        assert_eq!(
            result
                .recv_timeout(Duration::from_secs(10))
                .expect("wait ends"),
            Err(if cause == 2 {
                Error::Timeout
            } else {
                Error::Ended
            })
        );
        worker.join().expect("reader");
    }
}

#[test]
fn completed_stamp_does_not_override_terminal_control_or_eof() {
    let barrier = Barrier::new(None);
    barrier.complete();
    let control = Control::new(Instant::now() + Duration::from_secs(60));
    assert_eq!(barrier.wait_until(1, &control), Ok(()));
    control.cancel();
    assert_eq!(barrier.wait_until(1, &control), Err(Error::Ended));
    let fresh = Control::new(Instant::now() + Duration::from_secs(60));
    barrier.end();
    assert_eq!(barrier.wait_until(1, &fresh), Err(Error::Ended));
}
