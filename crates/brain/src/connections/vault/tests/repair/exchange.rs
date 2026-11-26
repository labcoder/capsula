use super::*;

use std::future::Future as _;
use std::sync::Arc;
use std::task::{Context, Waker};

use crate::connections::authority::Current;
use crate::connections::oauth::exchange::{Scripted, authorization, tokens};
use crate::connections::records::MAX_TOKEN_BYTES;
use crate::connections::vault::repair::exchange::RepairSettlement;
use crate::connections::vault::repair::{ExchangeIntake, RepairAttempt};

pub(super) fn admitted(
    vault: &mut Vault<FakeStore>,
    store: &FakeStore,
) -> (Current, RepairAttempt) {
    vault.host_id().unwrap();
    committed(vault.save_account(&account("old")));
    store.delete(&account_chunk(vault, 0)).unwrap();
    let head = vault.authority.current(Kind::Account).unwrap();
    let old = vault.missing_repair_head(head.revision).unwrap();
    let attempt = vault.confirm_repair(old, || true).unwrap();
    (old, attempt)
}

fn settlement(path: &Path) -> (String, bool) {
    raw(path)
        .query_row(
            "SELECT phase, opened_in=0 FROM chatgpt_repair WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

#[tokio::test]
async fn retiring_either_owner_suppresses_unsent_work_or_keeps_the_charge_unresolved() {
    for claim in [false, true] {
        for retire_vault in [false, true] {
            let (_scratch, path, service, store) = setup("repair-retired-owner");
            let mut vault = open(&store, &service, &path);
            let (_, attempt) = admitted(&mut vault, &store);
            let dispatch = vault
                .charge_repair_exchange(&attempt, &authorization())
                .unwrap();
            let resume = Arc::new(tokio::sync::Notify::new());
            let scripted = Scripted::new(200, serde_json::to_vec(&tokens()).unwrap())
                .with_pause(Arc::clone(&resume));
            let mut pending = std::pin::pin!(dispatch.receive(&scripted));
            if claim {
                let mut context = Context::from_waker(Waker::noop());
                assert!(pending.as_mut().poll(&mut context).is_pending());
                assert_eq!(scripted.request_count(), 1);
            }
            if retire_vault {
                drop(vault);
                assert!(matches!(
                    Vault::with_service(store, &service, &path),
                    Err(VaultError::Authority)
                ));
            } else {
                drop(attempt);
                assert!(matches!(vault.load_account(), Err(VaultError::Authority)));
            }
            resume.notify_one();
            let _receipt = pending.await;
            assert_eq!(scripted.request_count(), usize::from(claim));
            assert_eq!(settlement(&path), ("exchange".into(), false));
        }
    }
}

#[tokio::test]
async fn invalid_callback_does_not_spend_the_admitted_slot() {
    let (_scratch, path, service, store) = setup("repair-invalid-callback");
    let mut vault = open(&store, &service, &path);
    let (_, attempt) = admitted(&mut vault, &store);
    let names = store.names();
    let mut invalid = authorization();
    invalid.code = Secret::new(Zeroizing::new("x".repeat(MAX_TOKEN_BYTES + 1)));
    let scripted = Scripted::new(200, serde_json::to_vec(&tokens()).unwrap());
    assert!(matches!(
        vault.charge_repair_exchange(&attempt, &invalid),
        Err(VaultError::Fenced)
    ));
    assert_eq!(settlement(&path), ("browser".into(), false));
    assert_eq!(store.names(), names);
    assert_eq!(scripted.request_count(), 0);
    let dispatch = vault
        .charge_repair_exchange(&attempt, &authorization())
        .unwrap();
    let _receipt = dispatch.receive(&scripted).await;
    assert_eq!(scripted.request_count(), 1);
}

#[tokio::test]
async fn cancelling_before_dispatch_sends_nothing_and_spends_the_charge_once() {
    let (_scratch, path, service, store) = setup("repair-unsent");
    let mut vault = open(&store, &service, &path);
    let (old, first) = admitted(&mut vault, &store);
    vault.cancel_repair_admission(&first).unwrap();
    assert!(matches!(
        vault.charge_repair_exchange(&first, &authorization()),
        Err(VaultError::Fenced)
    ));

    let mut second = vault.confirm_repair(old, || true).unwrap();
    let dispatch = vault
        .charge_repair_exchange(&second, &authorization())
        .unwrap();
    assert_eq!(settlement(&path), ("exchange".into(), false));
    assert!(vault.confirm_repair(old, || true).is_err());
    assert!(matches!(vault.load_account(), Err(VaultError::Authority)));
    assert!(matches!(
        vault.charge_repair_exchange(&second, &authorization()),
        Err(VaultError::Authority)
    ));
    vault.cancel_repair_admission(&second).unwrap();
    let scripted = Scripted::new(200, serde_json::to_vec(&tokens()).unwrap());
    let mut receipt = dispatch.receive(&scripted).await;
    assert_eq!(scripted.request_count(), 0);
    assert!(matches!(
        vault.settle_repair_exchange(&mut second, &mut receipt),
        Ok(RepairSettlement::NotSent)
    ));
    assert!(matches!(
        vault.settle_repair_exchange(&mut second, &mut receipt),
        Err(VaultError::Fenced)
    ));
    assert_eq!(settlement(&path), ("unknown".into(), true));
    assert!(matches!(vault.load_account(), Err(VaultError::Authority)));
}

#[tokio::test]
async fn a_canceled_unusable_exchange_keeps_its_exact_issued_handle() {
    let (_scratch, path, service, store) = setup("repair-canceled-receipt");
    let mut vault = open(&store, &service, &path);
    let (old, mut earlier) = admitted(&mut vault, &store);
    vault.cancel_repair_admission(&earlier).unwrap();
    let mut attempt = vault.confirm_repair(old, || true).unwrap();
    let dispatch = vault
        .charge_repair_exchange(&attempt, &authorization())
        .unwrap();

    let mut response = tokens();
    response.as_object_mut().unwrap().remove("id_token");
    let resume = Arc::new(tokio::sync::Notify::new());
    let scripted =
        Scripted::new(200, serde_json::to_vec(&response).unwrap()).with_pause(Arc::clone(&resume));
    let mut pending = std::pin::pin!(dispatch.receive(&scripted));
    let mut context = Context::from_waker(Waker::noop());
    assert!(pending.as_mut().poll(&mut context).is_pending());
    assert_eq!(scripted.request_count(), 1);
    vault.cancel_repair_admission(&attempt).unwrap();
    resume.notify_one();
    let mut receipt = pending.await;
    assert!(!format!("{receipt:?}").contains("fixture-refresh"));
    let (_other, other_path, other_service, other_store) = setup("other-vault");
    let mut other = open(&other_store, &other_service, &other_path);
    assert!(matches!(
        other.settle_repair_exchange(&mut attempt, &mut receipt),
        Err(VaultError::Fenced)
    ));
    assert!(matches!(
        vault.settle_repair_exchange(&mut earlier, &mut receipt),
        Err(VaultError::Fenced)
    ));
    assert!(matches!(
        vault.settle_repair_exchange(&mut attempt, &mut receipt),
        Ok(RepairSettlement::Escrowed)
    ));
    assert!(matches!(
        vault.settle_repair_exchange(&mut attempt, &mut receipt),
        Err(VaultError::Fenced)
    ));
    assert_eq!(settlement(&path), ("escrow".into(), true));
    let active: String = raw(&path)
        .query_row(
            "SELECT lower(hex(credential)) FROM chatgpt_repair WHERE id=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let name = generation_target(&service, "repair", &active, 0);
    let bytes = store.get(&name).unwrap().unwrap();
    assert_eq!(
        bytes.expose().as_slice(),
        b"\x0d\x00issued-clientfixture-refresh"
    );
    assert!(matches!(vault.load_account(), Err(VaultError::Authority)));
    assert!(vault.load_revocations().unwrap().is_none());
}

#[tokio::test]
async fn retired_repair_client_and_other_provider_key_sweep_together() {
    let (_scratch, path, service, store) = setup("repair-mixed-sweep");
    let mut vault = open(&store, &service, &path);
    vault.save_anthropic_key(b"old-key").unwrap();
    let old_key = vault
        .authority
        .current(Kind::AnthropicKey)
        .unwrap()
        .active
        .unwrap();
    let old_key = generation_target(
        &service,
        Kind::AnthropicKey.label(),
        &old_key.generation.hex(),
        0,
    );
    let (_, mut attempt) = admitted(&mut vault, &store);
    let dispatch = vault
        .charge_repair_exchange(&attempt, &authorization())
        .unwrap();
    let client: String = raw(&path)
        .query_row(
            "SELECT lower(hex(credential)) FROM chatgpt_repair WHERE id=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let client = generation_target(&service, "repair", &client, 0);
    let scripted = Scripted::new(200, serde_json::to_vec(&tokens()).unwrap());
    let mut receipt = dispatch.receive(&scripted).await;
    assert!(matches!(
        vault.settle_repair_exchange(&mut attempt, &mut receipt),
        Ok(RepairSettlement::Escrowed)
    ));
    assert!(store.get(&client).unwrap().is_some());
    assert!(store.get(&old_key).unwrap().is_some());
    vault.save_anthropic_key(b"new-key").unwrap();
    assert!(store.get(&client).unwrap().is_none());
    assert!(store.get(&old_key).unwrap().is_none());
    assert_eq!(
        vault
            .read_bytes(Kind::AnthropicKey)
            .unwrap()
            .unwrap()
            .expose()
            .as_slice(),
        b"new-key"
    );
    assert_eq!(settlement(&path), ("escrow".into(), false));
}

#[tokio::test]
async fn missing_or_refused_settlement_stays_fenced_without_retry() {
    for issued in [false, true] {
        let (_scratch, path, service, store) = setup("repair-settlement-error");
        let mut vault = open(&store, &service, &path);
        let (old, mut attempt) = admitted(&mut vault, &store);
        let dispatch = vault
            .charge_repair_exchange(&attempt, &authorization())
            .unwrap();
        let scripted = if issued {
            Scripted::new(200, serde_json::to_vec(&tokens()).unwrap())
        } else {
            Scripted::new(500, b"{}".to_vec())
        };
        let mut receipt = dispatch.receive(&scripted).await;
        assert_eq!(scripted.request_count(), 1);
        store.0.lock().unwrap().refusing = issued;

        let settled = vault.settle_repair_exchange(&mut attempt, &mut receipt);
        if issued {
            assert!(matches!(
                settled,
                Err(VaultError::Store(CredentialStoreError::Write(5)))
            ));
            let ExchangeIntake::Held(handle) = &attempt.intake else {
                panic!("a refused write must not drop the issued handle");
            };
            assert_eq!(handle.expose().as_str(), "fixture-refresh");
        } else {
            assert!(matches!(settled, Ok(RepairSettlement::Unknown)));
        }
        assert_eq!(settlement(&path), ("unknown".into(), true));
        store.0.lock().unwrap().refusing = false;
        assert!(matches!(
            vault.settle_repair_exchange(&mut attempt, &mut receipt),
            Err(VaultError::Fenced)
        ));
        assert!(vault.confirm_repair(old, || true).is_err());
        assert!(matches!(vault.load_account(), Err(VaultError::Authority)));
    }
}
