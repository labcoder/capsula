use super::link::*;
use kumochi_protocol::message::{GitHubView, UserCode};

#[test]
fn github_event_debug_never_exposes_the_device_code() {
    let marker = "ZQX-PRIVATE-CODE";
    let event = LinkEvent::GitHub {
        generation: 7,
        view: GitHubView::AwaitingUser {
            user_code: UserCode::new(marker).unwrap(),
            expires_in_s: 30,
        },
    };
    let debug = format!("{:?}", Batch::Plain(vec![event]));
    assert!(!debug.contains(marker));
    assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
    let mut settings = crate::settings::Settings::default();
    assert!(settings.github.receive(
        Some(7),
        7,
        GitHubView::AwaitingUser {
            user_code: UserCode::new(marker).unwrap(),
            expires_in_s: 30,
        },
        std::time::Instant::now()
    ));
    let debug = format!("{settings:?}");
    assert!(!debug.contains(marker));
    assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
}
