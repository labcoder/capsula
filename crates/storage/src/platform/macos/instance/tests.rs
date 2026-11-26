use super::*;
use objc2_core_foundation::CFNumber;

const PREFIX: &str = "PREFIX";
const GROUP: &str = "PREFIX.ai.kumochi.dev.instance";

fn item() -> InstanceItem {
    InstanceItem {
        group: GROUP.to_owned(),
    }
}

type Pairs<'a> = Vec<(&'a CFString, &'a CFType)>;

fn pairs(dict: &ItemQuery) -> usize {
    dict.len()
}

fn holds(dict: &CFDictionary<CFString, CFType>, key: &CFString, want: &CFType) -> bool {
    dict.get(key).is_some_and(|value| *value == *want)
}

#[test]
fn every_query_pins_the_exact_item_without_ui_and_the_update_payload_holds_only_the_value() {
    let k = keys();
    let value = CFData::from_bytes(&[7; INSTANCE_LEN]);
    let group = CFString::from_static_str(GROUP);
    let service = CFString::from_static_str("Kumochi Dev instance");
    let account = CFString::from_static_str("capability");
    let pinned: [(&CFString, &CFType); 7] = [
        (k.class, k.generic_password.as_ref()),
        (k.group, group.as_ref()),
        (k.service, service.as_ref()),
        (k.account, account.as_ref()),
        (k.data_protection, k.yes),
        (k.synchronizable, k.no),
        (k.ui, k.ui_fail.as_ref()),
    ];
    let item = item();
    let cases: [(&str, ItemQuery, Pairs<'_>); 4] = [
        (
            "add",
            item.add_attributes(&value),
            vec![
                (k.accessible, k.protection.as_ref()),
                (k.value, value.as_ref()),
            ],
        ),
        (
            "inspect",
            item.inspect_query(),
            vec![(k.return_attributes, k.yes), (k.match_limit, k.one)],
        ),
        (
            "read back",
            item.read_query(),
            vec![
                (k.return_data, k.yes),
                (k.return_attributes, k.yes),
                (k.match_limit, k.one),
            ],
        ),
        ("update query and delete", item.exact(), vec![]),
    ];
    for (name, dict, extra) in &cases {
        assert_eq!(
            pairs(dict),
            pinned.len() + extra.len(),
            "{name}: no other key"
        );
        for (key, want) in pinned.iter().chain(extra) {
            assert!(holds(dict, key, want), "{name}: {key}");
        }
    }
    let payload = InstanceItem::update_payload(&value);
    assert_eq!(pairs(&payload), 1, "the update changes the value alone");
    assert!(holds(&payload, k.value, value.as_ref()));
    assert!(!payload.contains_key(k.ui), "no UI switch in the payload");
}

#[test]
fn admission_needs_this_roles_exact_id_and_the_instance_group_under_the_same_prefix() {
    let ids = AppleIds::DEV;
    let text = |value: &str| CFString::from_str(value);
    let groups = |entries: &[&str]| {
        let entries: Vec<_> = entries.iter().map(|entry| text(entry)).collect();
        let entries: Vec<&CFType> = entries.iter().map(|entry| -> &CFType { entry }).collect();
        CFArray::from_objects(&entries)
    };
    let admit = |role, app_id: &CFType, groups: &CFType| {
        admitted_group(ids, role, Some(app_id), Some(groups))
    };
    let both = groups(&["PREFIX.ai.kumochi.dev.brain", GROUP]);
    let brain = text("PREFIX.ai.kumochi.dev.brain");
    assert_eq!(admit(Role::Brain, &brain, &both), Ok(GROUP.to_owned()));
    let shell_groups = groups(&[GROUP]);
    assert_eq!(
        admit(
            Role::Shell,
            &text("PREFIX.ai.kumochi.dev.shell"),
            &shell_groups
        ),
        Ok(GROUP.to_owned())
    );
    let other = groups(&["QRSTU67890.ai.kumochi.dev.instance"]);
    assert_eq!(
        admit(
            Role::Brain,
            &text("QRSTU67890.ai.kumochi.dev.brain"),
            &other
        ),
        Ok("QRSTU67890.ai.kumochi.dev.instance".to_owned()),
        "a provisioned prefix that isn't the team's is still the prefix"
    );
    for app_id in [
        ".ai.kumochi.dev.brain",
        "ai.kumochi.dev.brain",
        "PREFIX.ai.kumochi.dev.shell",
        "PREFIX.ai.kumochi.brain",
        "*.ai.kumochi.dev.brain",
        "PREFIX*.ai.kumochi.dev.brain",
        "PREFIX.*",
        "PREFIX.ai.kumochi.dev.brainx",
    ] {
        assert_eq!(
            admit(Role::Brain, &text(app_id), &both),
            Err(ItemError::NotAdmitted),
            "{app_id}"
        );
    }
    for (app_id, group) in [
        (".ai.kumochi.dev.brain", ".ai.kumochi.dev.instance"),
        ("*.ai.kumochi.dev.brain", "*.ai.kumochi.dev.instance"),
        (
            "PREFIX.extra.ai.kumochi.dev.brain",
            "PREFIX.extra.ai.kumochi.dev.instance",
        ),
    ] {
        assert_eq!(
            admit(Role::Brain, &text(app_id), &groups(&[group])),
            Err(ItemError::NotAdmitted),
            "{app_id} is refused even when its group is listed"
        );
    }
    let number = CFNumber::new_i32(7);
    assert_eq!(
        admit(Role::Brain, &number, &both),
        Err(ItemError::NotAdmitted)
    );
    assert_eq!(
        admitted_group(ids, Role::Brain, None, Some(&both)),
        Err(ItemError::NotAdmitted)
    );
    assert_eq!(
        admit(Role::Brain, &brain, &text(GROUP)),
        Err(ItemError::NotAdmitted),
        "groups must be an array"
    );
    let group = text(GROUP);
    let mixed = CFArray::<CFType>::from_objects(&[&number, &group]);
    assert_eq!(
        admit(Role::Brain, &brain, &mixed),
        Err(ItemError::NotAdmitted),
        "every group is a string"
    );
    assert_eq!(
        admitted_group(ids, Role::Brain, Some(&brain), None),
        Err(ItemError::NotAdmitted)
    );
    for entries in [
        &["OTHER.ai.kumochi.dev.instance"][..],
        &["PREFIX.*"],
        &["PREFIX.ai.kumochi.dev.brain"],
        &[],
    ] {
        assert_eq!(
            admit(Role::Brain, &brain, &groups(entries)),
            Err(ItemError::NotAdmitted),
            "{entries:?}"
        );
    }
    assert_eq!(PREFIX.len() + 1 + ids.instance_group.len(), GROUP.len());
}

#[test]
fn only_dev_has_an_item() {
    assert_eq!(instance_edition(ProductIdentity::DEV), Ok(AppleIds::DEV));
    assert_eq!(
        instance_edition(ProductIdentity::RELEASE),
        Err(ItemError::NotAdmitted)
    );
    assert_eq!(
        instance_edition(ProductIdentity::GATE),
        Err(ItemError::NotAdmitted)
    );
}

#[test]
fn statuses_map_by_operation() {
    for op in [Op::Read, Op::Write, Op::Delete] {
        assert_eq!(status(op, 0), Ok(()));
        assert_eq!(status(op, -25308), Err(ItemError::Unavailable));
        assert_eq!(status(op, -34018), Err(ItemError::NotAdmitted));
        assert_eq!(status(op, -25299), Err(ItemError::Failed(-25299)));
        assert_eq!(status(op, -50), Err(ItemError::Failed(-50)));
    }
    assert_eq!(status(Op::Read, -25300), Err(ItemError::Missing));
    assert_eq!(status(Op::Delete, -25300), Err(ItemError::Absent));
    assert_eq!(status(Op::Write, -25300), Err(ItemError::Failed(-25300)));
}

fn attributes(pairs: &[(&CFString, &CFType)]) -> CFRetained<CFDictionary<CFString, CFType>> {
    let (keys, values): (Vec<_>, Vec<_>) = pairs.iter().copied().unzip();
    CFDictionary::from_slices(&keys, &values)
}

#[test]
fn only_a_plain_protected_item_passes_and_reads_back_exactly_32_bytes() {
    let k = keys();
    // SAFETY: reads one immutable Security framework constant that the loader initialized.
    let migratable = unsafe { objc2_security::kSecAttrAccessibleAfterFirstUnlock };
    let bytes: Vec<u8> = (1..=32).collect();
    let value = CFData::from_bytes(&bytes);
    let protected = (k.accessible, k.protection.as_ref());
    let plain = attributes(&[protected, (k.value, value.as_ref())]);
    assert!(protection(plain.as_ref()).is_ok());
    let mut into = [0_u8; INSTANCE_LEN];
    assert_eq!(readback(plain.as_ref(), &mut into), Ok(()));
    assert_eq!(into.to_vec(), bytes);

    let acl = CFString::from_static_str("acl");
    let number = CFNumber::new_i32(1);
    let text = CFString::from_static_str("value");
    let refused = [
        (
            "migratable class",
            attributes(&[
                (k.accessible, migratable.as_ref()),
                (k.value, value.as_ref()),
            ]),
        ),
        (
            "access control",
            attributes(&[
                protected,
                (k.access_control, acl.as_ref()),
                (k.value, value.as_ref()),
            ]),
        ),
        ("no class", attributes(&[(k.value, value.as_ref())])),
        (
            "mistyped class",
            attributes(&[(k.accessible, number.as_ref()), (k.value, value.as_ref())]),
        ),
    ];
    for (name, item) in &refused {
        assert_eq!(
            protection(item.as_ref()).map(drop),
            Err(ItemError::Ineligible),
            "{name}"
        );
        let mut into = [0xEE_u8; INSTANCE_LEN];
        assert_eq!(
            readback(item.as_ref(), &mut into),
            Err(ItemError::Ineligible),
            "{name}"
        );
        assert_eq!(into, [0xEE; INSTANCE_LEN], "{name}: nothing copied");
    }
    assert_eq!(
        protection(text.as_ref()).map(drop),
        Err(ItemError::Ineligible),
        "not a dictionary"
    );

    let short = CFData::from_bytes(&bytes[..31]);
    let long = CFData::from_bytes(&[bytes.as_slice(), &[33]].concat());
    for (name, item) in [
        ("no value", attributes(&[protected])),
        (
            "mistyped value",
            attributes(&[protected, (k.value, text.as_ref())]),
        ),
        (
            "31 bytes",
            attributes(&[protected, (k.value, short.as_ref())]),
        ),
        (
            "33 bytes",
            attributes(&[protected, (k.value, long.as_ref())]),
        ),
    ] {
        assert!(
            protection(item.as_ref()).is_ok(),
            "{name}: the attributes alone pass"
        );
        let mut into = [0xEE_u8; INSTANCE_LEN];
        assert_eq!(
            readback(item.as_ref(), &mut into),
            Err(ItemError::Ineligible),
            "{name}"
        );
        assert_eq!(into, [0xEE; INSTANCE_LEN], "{name}: nothing copied");
    }
}

#[test]
fn the_item_prints_no_group() {
    assert_eq!(format!("{:?}", item()), "InstanceItem(..)");
}
