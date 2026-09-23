//! Recover inline input headers omitted by the SDK's SyncTransactions decoder.
//! The stock block RPC preserves them; no node or client fork is required.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, ensure};
use miden_client::rpc::{NodeRpcClient, domain::transaction::TransactionRecord};
use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;
use miden_protocol::transaction::TransactionHeader;

fn needs_input_headers(record: &TransactionRecord) -> bool {
    let known: BTreeSet<_> = record
        .trusted_consumed_note_refs()
        .map(|(nf, _)| nf)
        .collect();
    record
        .transaction_header
        .input_notes()
        .iter()
        .any(|input| input.header().is_none() && !known.contains(&input.nullifier()))
}

/// Fetch only blocks containing unidentified inputs, once per block per pass.
/// Existing RPC authentication, retries and process-wide pacing still apply.
pub(crate) async fn recover_input_headers(
    rpc: &dyn NodeRpcClient,
    records: &mut [TransactionRecord],
    account: AccountId,
) -> anyhow::Result<()> {
    let mut blocks = BTreeMap::<BlockNumber, Vec<usize>>::new();
    for (index, record) in records.iter().enumerate() {
        if record.transaction_header.account_id() == account && needs_input_headers(record) {
            blocks.entry(record.block_num).or_default().push(index);
        }
    }
    for (number, indices) in blocks {
        let block = rpc
            .get_block_by_number(number, false)
            .await
            .with_context(|| format!("recover input headers at block {number}"))?;
        ensure!(
            block.header().block_num() == number,
            "input header recovery returned the wrong block"
        );
        ensure!(
            block.header().tx_commitment() == block.body().transactions().commitment(),
            "input header recovery: block transaction commitment mismatch"
        );
        for index in indices {
            restore_record_header(&mut records[index], block.body().transactions().as_slice())?;
        }
    }
    Ok(())
}

fn restore_record_header(
    record: &mut TransactionRecord,
    headers: &[TransactionHeader],
) -> anyhow::Result<()> {
    let decoded = &record.transaction_header;
    // The SDK also recomputes the transaction ID after discarding headers.
    // Match the retained fields, including ordered nullifiers, instead of that ID.
    let mut matches = headers.iter().filter(|header| {
        header.account_id() == decoded.account_id()
            && header.initial_state_commitment() == decoded.initial_state_commitment()
            && header.final_state_commitment() == decoded.final_state_commitment()
            && header.output_notes() == decoded.output_notes()
            && header
                .input_notes()
                .iter()
                .map(|n| n.nullifier())
                .eq(decoded.input_notes().iter().map(|n| n.nullifier()))
    });
    let header = matches
        .next()
        .context("block lacks the matching SyncTransactions record")?;
    ensure!(
        matches.next().is_none(),
        "block contains ambiguous transaction headers"
    );
    for (old, recovered) in decoded
        .input_notes()
        .iter()
        .zip(header.input_notes().iter())
    {
        if let Some(existing) = old.header() {
            ensure!(
                Some(existing) == recovered.header(),
                "block conflicts with an existing input header"
            );
        }
    }
    record.transaction_header = header.clone();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use miden_protocol::note::{
        NoteAttachments, NoteHeader, NoteMetadata, NoteTag, NoteType, Nullifier,
        PartialNoteMetadata,
    };
    use miden_protocol::transaction::{InputNoteCommitment, InputNotes};
    use miden_protocol::{Felt, Word};

    fn fixture(tag: u32) -> (TransactionRecord, TransactionHeader, NoteHeader) {
        let account = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        let word = |n| Word::new([Felt::new(n).unwrap(); 4]);
        let nf = Nullifier::from_raw(word(1));
        let note = NoteHeader::new(
            miden_protocol::note::NoteDetailsCommitment::from_raw_commitments(word(2), word(5)),
            NoteMetadata::new(
                PartialNoteMetadata::new(account, NoteType::Public).with_tag(NoteTag::from(tag)),
                &NoteAttachments::default(),
            ),
        );
        let decoded = crate::test_helpers::test_tx_record(
            133078,
            account,
            word(3),
            word(4),
            vec![nf],
            vec![],
        );
        let full = TransactionHeader::new(
            account,
            word(3),
            word(4),
            InputNotes::new(vec![InputNoteCommitment::from_parts_unchecked(
                nf,
                Some(note),
            )])
            .unwrap(),
            vec![],
        );
        (decoded, full, note)
    }

    #[test]
    fn stock_block_recovers_headers_for_tag_zero_and_funding() {
        for tag in [0, 0x5c10_0000] {
            let (mut record, full, note) = fixture(tag);
            assert!(needs_input_headers(&record));
            assert_ne!(record.transaction_header.id(), full.id());
            restore_record_header(&mut record, std::slice::from_ref(&full)).unwrap();
            assert_eq!(record.transaction_header, full);
            assert_eq!(
                record.transaction_header.input_notes().get_note(0).header(),
                Some(&note)
            );
            assert!(!needs_input_headers(&record));
        }
    }

    #[test]
    fn missing_or_ambiguous_block_transaction_fails_closed() {
        let (mut record, full, _) = fixture(0);
        let original = record.transaction_header.clone();
        assert!(restore_record_header(&mut record, &[]).is_err());
        assert!(restore_record_header(&mut record, &[full.clone(), full]).is_err());
        assert_eq!(record.transaction_header, original);
    }

    #[test]
    fn conflicting_existing_header_fails_closed() {
        let (mut record, full, _) = fixture(0);
        record.transaction_header = full;
        let (_, funding, _) = fixture(0x5c10_0000);
        assert!(restore_record_header(&mut record, &[funding]).is_err());
    }

    #[test]
    fn changed_input_order_cannot_supply_a_header() {
        let (mut record, full, _) = fixture(0);
        let first = full.input_notes().get_note(0).clone();
        let second =
            InputNoteCommitment::from(Nullifier::from_raw(Word::new([Felt::new(9).unwrap(); 4])));
        record.transaction_header = TransactionHeader::new(
            full.account_id(),
            full.initial_state_commitment(),
            full.final_state_commitment(),
            InputNotes::new(vec![
                InputNoteCommitment::from(first.nullifier()),
                second.clone(),
            ])
            .unwrap(),
            vec![],
        );
        let reversed = TransactionHeader::new(
            full.account_id(),
            full.initial_state_commitment(),
            full.final_state_commitment(),
            InputNotes::new(vec![second, first]).unwrap(),
            vec![],
        );
        assert!(restore_record_header(&mut record, &[reversed]).is_err());
    }

    #[test]
    fn unresolved_authenticated_input_stays_unresolved() {
        let (mut record, _, _) = fixture(0);
        let header = record.transaction_header.clone();
        restore_record_header(&mut record, &[header]).unwrap();
        assert!(needs_input_headers(&record));
        assert!(
            record
                .transaction_header
                .input_notes()
                .get_note(0)
                .header()
                .is_none()
        );
    }
}
