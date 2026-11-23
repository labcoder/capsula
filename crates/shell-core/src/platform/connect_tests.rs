use super::*;

#[test]
fn a_missing_pipe_only_launches_again_after_a_build_mismatch() {
    let name = format!(
        r"\\.\pipe\{}-test-connect-{}",
        kumochi_protocol::constants::OBJECT_PREFIX,
        std::process::id()
    );
    let brain = std::env::current_exe().unwrap();

    let (server, client) = super::super::test_pair(&name).unwrap();
    assert!(matches!(open(&name, &brain, None), Err(LinkError::Busy)));
    drop((client, server));
    assert!(matches!(open(&name, &brain, None), Err(LinkError::NoBrain)));

    let launches = std::cell::Cell::new(0);
    let mut outcomes = [
        (0, LinkError::NoBrain),
        (1, LinkError::NoBrain),
        (1, LinkError::Busy),
        (1, LinkError::BuildMismatch),
        (1, LinkError::NoBrain),
        (2, LinkError::NotOurBrain),
    ]
    .into_iter();
    let result = connect_with(
        AudioSession::Legacy,
        Instant::now() + Duration::from_secs(5),
        || false,
        |_| panic!("not quitting"),
        |attempt| {
            assert_eq!(attempt, None);
            let (launched, error) = outcomes.next().expect("unexpected retry after refusal");
            assert_eq!(launches.get(), launched);
            Err(error)
        },
        || {
            launches.set(launches.get() + 1);
            Ok(())
        },
    );
    assert!(matches!(result, Err(LinkError::NotOurBrain)));
    assert_eq!(launches.get(), 2);
    assert!(outcomes.next().is_none());
}
