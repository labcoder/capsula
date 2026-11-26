use super::*;

#[test]
fn a_view_releases_its_wiped_backing_when_its_last_reference_drops() {
    let key = [0xA5_u8; CAPABILITY_LEN];
    let before = RELEASED.get();
    let view = owned_view(&key).expect("a view");
    assert_eq!(view.to_vec(), key.to_vec());
    let second = view.clone();
    drop(view);
    assert_eq!(RELEASED.get(), before, "a live reference keeps the backing");
    drop(second);
    assert_eq!(
        RELEASED.get(),
        before + 1,
        "the backing was released, and so wiped"
    );
}

#[test]
#[ignore = "calls SecTask on this process: run only in the signed, provisioned Dev brain host"]
fn signed_host_admits_the_dev_brain_before_any_item_call() {
    assert!(KeychainItem::admit().is_ok());
}
