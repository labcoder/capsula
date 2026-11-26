//! What parent-bound admission writes on the inherited pair, read from the shell's ends.

use std::io;

use kumochi_session::{Channel, ShellAwaitsReply};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::respond;
use super::tests::{SECRET, bounded, brain, by, ends};

#[tokio::test]
async fn ready_is_one_control_byte_and_the_responder_speaks_only_after_the_shells_first_message() {
    let (pair, [mut control, data]) = ends();
    assert_eq!(pair.try_ready().expect("READY written"), 1);
    let mut first = [0; 1];
    bounded(control.read_exact(&mut first))
        .await
        .expect("READY read");
    assert_eq!(first, [0x52]);
    let mut more = [0; 1];
    for end in [&control, &data] {
        assert_eq!(
            end.try_read(&mut more).map_err(|error| error.kind()),
            Err(io::ErrorKind::WouldBlock),
            "nothing else is written before the shell's first message"
        );
    }
    let (_shell, init) = ShellAwaitsReply::start(&SECRET, Channel::Dev).expect("shell starts");
    let responding = respond(pair, brain(), by());
    let envelope = bounded(async {
        tokio::select! {
            refused = responding => panic!("the responder ended early: {:?}", refused.err()),
            envelope = async {
                control.write_all(&init.bytes).await.expect("message 1");
                let mut envelope = [0; 2];
                control.read_exact(&mut envelope).await.expect("message 2's envelope");
                envelope
            } => envelope,
        }
    })
    .await;
    assert_eq!(envelope, [0x00, 0x30]);
}
