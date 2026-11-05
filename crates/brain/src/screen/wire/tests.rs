use super::*;

#[test]
fn request_debug_is_redacted() {
    assert_eq!(format!("{:?}", RequestBody::Ping), "RequestBody(***)");
}
