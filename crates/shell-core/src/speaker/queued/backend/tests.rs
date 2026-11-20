use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

#[test]
fn blocked_discovery_refuses_another_native_attempt_before_entering_it() {
    let (entered, first_entered) = mpsc::sync_channel(1);
    let (release, finish) = mpsc::sync_channel(1);
    let (first, first_reply) = spawn_with(thread::current(), Instant::now(), move |_, _| {
        entered.send(()).unwrap();
        finish.recv().unwrap();
        Err(OutputError::Unavailable)
    })
    .unwrap();
    assert!(first.send(Request::Open(1, Arc::default())).is_ok());
    first_entered.recv().unwrap();

    let attempted = Arc::new(AtomicBool::new(false));
    let count = Arc::clone(&attempted);
    let (second, second_reply) = spawn_with(thread::current(), Instant::now(), move |_, _| {
        count.store(true, Ordering::Release);
        Err(OutputError::Unavailable)
    })
    .unwrap();
    assert!(second.send(Request::Open(2, Arc::default())).is_ok());
    let result = second_reply.recv();
    release.send(()).unwrap();
    assert!(matches!(first_reply.recv(), Ok(Reply::Opened(1, None))));
    assert!(matches!(result, Ok(Reply::Opened(2, None))));
    assert!(!attempted.load(Ordering::Acquire));
}
