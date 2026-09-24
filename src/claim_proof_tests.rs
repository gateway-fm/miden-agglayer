use crate::store::{Store, TxnEntry};
use alloy::consensus::{Signed, TxEip1559, TxEnvelope};
use alloy::primitives::{Address, Signature, U256};
use std::time::Duration;

pub(crate) async fn invalid_claim_store_contract(store: &dyn Store) {
    for successor in [false, true] {
        let hash = alloy::primitives::keccak256(
            alloy::signers::local::PrivateKeySigner::random().address(),
        );
        let gi = U256::from_be_bytes(hash.0);
        let hash_string = format!("{hash:#x}");
        store
            .txn_begin(
                hash,
                TxnEntry {
                    id: None,
                    envelope: TxEnvelope::Eip1559(Signed::new_unchecked(
                        TxEip1559::default(),
                        Signature::test_signature(),
                        hash,
                    )),
                    signer: Address::ZERO,
                    expires_at: None,
                    logs: vec![],
                },
            )
            .await
            .unwrap();
        let owner = if successor {
            alloy::primitives::keccak256(
                alloy::signers::local::PrivateKeySigner::random().address(),
            )
        } else {
            hash
        };
        let fence = store
            .try_claim_fenced(gi, owner, Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();
        if successor {
            store
                .prepare_note_handoff(&hash_string, &hash_string, "0xexact-note", 100)
                .await
                .unwrap();
        } else {
            assert!(
                store
                    .prepare_claim_submission_fenced(
                        gi,
                        hash,
                        fence.fence,
                        hash,
                        &hash_string,
                        "0xexact-note",
                        100
                    )
                    .await
                    .unwrap()
            );
        }
        store
            .confirm_note_handoff(&hash_string, &hash_string)
            .await
            .unwrap();

        assert!(
            !store
                .txn_fail_invalid_claim(hash, gi, Some("wrong-note"), "InvalidSmtProof", 101)
                .await
                .unwrap()
        );
        assert!(store.txn_receipt(hash).await.unwrap().is_none());
        assert!(store.is_claimed(&gi).await.unwrap());
        assert!(
            store
                .txn_fail_invalid_claim(hash, gi, Some(&hash_string), "InvalidSmtProof", 101)
                .await
                .unwrap()
        );
        assert_eq!(
            store.txn_receipt(hash).await.unwrap().unwrap(),
            (Err("InvalidSmtProof".into()), 101)
        );
        assert!(
            store
                .get_note_handoff_for_tx(&hash_string)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(store.is_claimed(&gi).await.unwrap(), successor);
        assert!(store.get_unclaimable_claim(&gi).await.unwrap().is_none());
        assert!(
            !store
                .has_claim_event_for_global_index(&gi.to_be_bytes::<32>())
                .await
                .unwrap()
        );

        // Idempotent retries cannot alter a terminal receipt or release a new owner.
        let new_owner = alloy::primitives::keccak256(
            alloy::signers::local::PrivateKeySigner::random().address(),
        );
        if !successor {
            assert!(
                store
                    .try_claim_fenced(gi, new_owner, Duration::from_secs(60))
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        assert!(
            !store
                .txn_fail_invalid_claim(hash, gi, Some(&hash_string), "changed", 102)
                .await
                .unwrap()
        );
        assert!(store.is_claimed(&gi).await.unwrap());
        assert_eq!(store.txn_receipt(hash).await.unwrap().unwrap().1, 101);
        if successor {
            assert!(
                store
                    .renew_claim_fenced(gi, owner, fence.fence, Duration::from_secs(60))
                    .await
                    .unwrap()
            );
        }
    }
}

#[tokio::test]
async fn invalid_claim_memory_store_fences_receipts_and_owners() {
    invalid_claim_store_contract(&crate::store::memory::InMemoryStore::new()).await;
}
