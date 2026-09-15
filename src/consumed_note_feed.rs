//! Incremental local consumed-note observations. Block cursors cannot capture
//! late imports or enrichment of old records, so an application-owned SQLite
//! change index observes the SDK's writes in the same transaction. All SQLite
//! access still runs through the one SDK owner; no second client or WAL policy
//! change is involved. A restart rebuilds the disposable reader from inventory.

use crate::client_access::ClientAccess;
use miden_client::store::{InputNoteRecord, InputNoteState, NoteFilter};
use miden_client::utils::Deserializable;
use miden_protocol::note::NoteDetailsCommitment;
use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags, params};
use std::collections::BTreeSet;

const READ_BATCH: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Mark {
    epoch: Vec<u8>,
    revision: i64,
}

#[derive(Default)]
pub(crate) struct ConsumedNoteFeed {
    reader: &'static str,
    mark: Mutex<Option<Mark>>,
    retry: Mutex<BTreeSet<[u8; 32]>>,
}

pub(crate) struct NoteBatch {
    pub notes: Vec<InputNoteRecord>,
    mark: Option<Mark>,
}

impl ConsumedNoteFeed {
    pub fn new(reader: &'static str) -> Self {
        Self {
            reader,
            ..Default::default()
        }
    }

    pub fn reset(&self) {
        *self.mark.lock() = None;
        self.retry.lock().clear();
    }

    pub async fn load(
        &self,
        client: &mut ClientAccess<'_>,
        force_full: bool,
        extra: &[[u8; 32]],
    ) -> anyhow::Result<NoteBatch> {
        let previous = self.mark.lock().clone();
        let requested = previous.clone();
        let changes = client
            .call("consumed_notes::read_changes", move |client| {
                Box::pin(async move {
                    let mut conn = Connection::open_with_flags(
                        client.store_identifier(),
                        OpenFlags::SQLITE_OPEN_READ_WRITE,
                    )?;
                    read_changes(&mut conn, requested.as_ref())
                })
            })
            .await;
        let (mark, mut keys, mut full) = match changes {
            Ok((mark, keys)) => {
                let full = previous.as_ref().is_none_or(|old| old.epoch != mark.epoch);
                (Some(mark), keys, full)
            }
            Err(error) => {
                metrics::counter!("consumed_note_feed_fallback_total", "reader" => self.reader)
                    .increment(1);
                tracing::warn!(target: "bridge_out::scan", reader = self.reader, error = %format!("{error:#}"),
                    "consumed-note change index unavailable; reading full inventory");
                (None, BTreeSet::new(), true)
            }
        };
        full |= force_full;
        let changed = keys.len();
        let retry = self.retry.lock().clone();
        keys.extend(retry.iter().copied());
        keys.extend(extra.iter().copied());
        let notes = if full {
            client.get_input_notes(NoteFilter::Consumed).await?
        } else {
            let keys: Vec<_> = keys.into_iter().collect();
            let mut notes = Vec::new();
            for chunk in keys.chunks(READ_BATCH) {
                let commitments = chunk
                    .iter()
                    .map(|key| NoteDetailsCommitment::read_from_bytes(key))
                    .collect::<Result<Vec<_>, _>>()?;
                notes.extend(
                    client
                        .get_input_notes(NoteFilter::DetailsCommitments(commitments))
                        .await?
                        .into_iter()
                        .filter(InputNoteRecord::is_consumed),
                );
            }
            notes
        };
        metrics::counter!("consumed_note_feed_reads_total", "reader" => self.reader,
            "mode" => if full { "full" } else { "incremental" })
        .increment(1);
        tracing::debug!(target: "bridge_out::scan", reader = self.reader, full_inventory = full,
            changed, retry = retry.len(), extra = extra.len(), loaded = notes.len(),
            revision = mark.as_ref().map(|m| m.revision),
            "consumed-note change batch loaded");
        Ok(NoteBatch { notes, mark })
    }

    /// Only call after the batch's work completes. Errors before this point
    /// replay the same changes; unresolved observations are explicitly retried.
    pub fn commit(&self, batch: &NoteBatch, retry: impl IntoIterator<Item = [u8; 32]>) {
        *self.retry.lock() = retry.into_iter().collect();
        *self.mark.lock() = batch.mark.clone();
        let pending = self.retry.lock().len();
        metrics::gauge!("consumed_note_feed_retry_notes", "reader" => self.reader)
            .set(pending as f64);
        tracing::debug!(target: "bridge_out::scan", reader = self.reader, retry = pending,
            revision = batch.mark.as_ref().map(|mark| mark.revision),
            "consumed-note change batch acknowledged");
    }
}

fn consumed_states() -> String {
    format!(
        "{}, {}, {}",
        InputNoteState::STATE_CONSUMED_AUTHENTICATED_LOCAL,
        InputNoteState::STATE_CONSUMED_UNAUTHENTICATED_LOCAL,
        InputNoteState::STATE_CONSUMED_EXTERNAL
    )
}

/// One compact row per changed details commitment, indexed by revision. The
/// metadata grows with distinct changed notes, not with the number of polls.
/// UPDATE captures every stored observation field, including headerless notes.
fn ensure_index(conn: &mut Connection) -> anyhow::Result<()> {
    let columns = [
        "note_id",
        "assets",
        "attachments",
        "serial_number",
        "inputs",
        "script_root",
        "nullifier",
        "state_discriminant",
        "state",
        "created_at",
        "consumed_block_height",
        "consumed_tx_order",
        "consumer_account_id",
        "details_commitment",
    ];
    // Refuse a changed SDK schema before installing triggers that could
    // break its writes or silently miss a newly added observation field.
    let actual: BTreeSet<String> = conn
        .prepare("PRAGMA table_info(input_notes)")?
        .query_map([], |row| row.get(1))?
        .collect::<Result<_, _>>()?;
    if actual != columns.iter().map(|column| (*column).to_owned()).collect() {
        conn.execute_batch("DROP TRIGGER IF EXISTS agglayer_note_insert_v1; DROP TRIGGER IF EXISTS agglayer_note_update_v1; DROP TRIGGER IF EXISTS agglayer_note_delete_v1;")?;
        anyhow::bail!("unsupported SDK input_notes schema; consumed-note observer disabled");
    }
    let installed: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name IN \
         ('agglayer_note_insert_v1','agglayer_note_update_v1','agglayer_note_delete_v1')",
        [],
        |row| row.get(0),
    )?;
    if installed == 3 {
        return Ok(());
    }
    let tx = conn.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS agglayer_note_change_state_v1 \
         (id INTEGER PRIMARY KEY CHECK(id=1), epoch BLOB NOT NULL, revision INTEGER NOT NULL); \
         CREATE TABLE IF NOT EXISTS agglayer_note_changes_v1 \
         (details_commitment BLOB PRIMARY KEY, revision INTEGER NOT NULL) WITHOUT ROWID; \
         CREATE INDEX IF NOT EXISTS agglayer_note_changes_revision_v1 \
         ON agglayer_note_changes_v1(revision); \
         INSERT INTO agglayer_note_change_state_v1 VALUES(1, randomblob(16), 0) \
         ON CONFLICT(id) DO UPDATE SET epoch=randomblob(16), revision=0; \
         DELETE FROM agglayer_note_changes_v1; \
         DROP TRIGGER IF EXISTS agglayer_note_insert_v1; \
         DROP TRIGGER IF EXISTS agglayer_note_update_v1; \
         DROP TRIGGER IF EXISTS agglayer_note_delete_v1;",
    )?;
    let states = consumed_states();
    let changed = columns
        .iter()
        .map(|c| format!("OLD.{c} IS NOT NEW.{c}"))
        .collect::<Vec<_>>()
        .join(" OR ");
    // The pinned SDK uses INSERT OR REPLACE. Compare BEFORE replacement,
    // while the old row is still visible; AFTER INSERT would requeue every
    // identical upsert and cannot observe consumed -> non-consumed resets.
    let unchanged = columns
        .iter()
        .map(|c| format!("prior.{c} IS NEW.{c}"))
        .collect::<Vec<_>>()
        .join(" AND ");
    let insert_condition = format!(
        "(NEW.state_discriminant IN ({states}) OR EXISTS (SELECT 1 FROM input_notes prior \
         WHERE prior.details_commitment=NEW.details_commitment AND prior.state_discriminant IN ({states}))) \
         AND NOT EXISTS (SELECT 1 FROM input_notes prior WHERE prior.details_commitment=NEW.details_commitment AND {unchanged})"
    );
    for (name, timing, event, row, condition) in [
        ("insert", "BEFORE", "INSERT", "NEW", insert_condition),
        (
            "update",
            "AFTER",
            "UPDATE",
            "NEW",
            format!(
                "(OLD.state_discriminant IN ({states}) OR NEW.state_discriminant IN ({states})) AND ({changed})"
            ),
        ),
        (
            "delete",
            "AFTER",
            "DELETE",
            "OLD",
            format!("OLD.state_discriminant IN ({states})"),
        ),
    ] {
        let retired_key = if name == "update" {
            "INSERT INTO agglayer_note_changes_v1(details_commitment, revision) \
             SELECT OLD.details_commitment, revision FROM agglayer_note_change_state_v1 \
             WHERE id=1 AND OLD.details_commitment IS NOT NEW.details_commitment \
             ON CONFLICT(details_commitment) DO UPDATE SET revision=excluded.revision;"
        } else {
            ""
        };
        tx.execute_batch(&format!(
            "CREATE TRIGGER agglayer_note_{name}_v1 {timing} {event} ON input_notes WHEN {condition} BEGIN \
             UPDATE agglayer_note_change_state_v1 SET revision=revision+1 WHERE id=1; \
             INSERT INTO agglayer_note_changes_v1(details_commitment, revision) \
             SELECT {row}.details_commitment, revision FROM agglayer_note_change_state_v1 WHERE id=1 \
             ON CONFLICT(details_commitment) DO UPDATE SET revision=excluded.revision; {retired_key} END;"
        ))?;
    }
    tx.commit()?;
    Ok(())
}

fn read_changes(
    conn: &mut Connection,
    previous: Option<&Mark>,
) -> anyhow::Result<(Mark, BTreeSet<[u8; 32]>)> {
    ensure_index(conn)?;
    let tx = conn.transaction()?;
    let mark = tx.query_row(
        "SELECT epoch, revision FROM agglayer_note_change_state_v1 WHERE id=1",
        [],
        |row| {
            Ok(Mark {
                epoch: row.get(0)?,
                revision: row.get(1)?,
            })
        },
    )?;
    let mut keys = BTreeSet::new();
    if let Some(old) = previous.filter(|old| old.epoch == mark.epoch) {
        anyhow::ensure!(
            old.revision <= mark.revision,
            "consumed-note change revision moved backwards"
        );
        let mut stmt = tx.prepare("SELECT details_commitment FROM agglayer_note_changes_v1 WHERE revision > ? AND revision <= ?")?;
        for row in stmt.query_map(params![old.revision, mark.revision], |row| {
            row.get::<_, Vec<u8>>(0)
        })? {
            keys.insert(
                row?.try_into()
                    .map_err(|_| anyhow::anyhow!("invalid changed-note commitment"))?,
            );
        }
    }
    tx.commit()?;
    Ok((mark, keys))
}

#[cfg(test)]
mod tests {
    use super::*;
    use miden_client::store::Store;
    use miden_client::store::input_note_states::{ConsumedExternalNoteState, ExpectedNoteState};
    use miden_client_sqlite_store::SqliteStore;
    use miden_protocol::account::AccountId;
    use miden_protocol::block::BlockNumber;
    use miden_protocol::note::{
        NoteAssets, NoteAttachments, NoteDetails, NoteMetadata, NoteRecipient, NoteStorage,
        NoteType, PartialNoteMetadata,
    };
    use miden_protocol::{Felt, Word};

    fn note(serial: u32, consumed: bool, enriched: bool) -> InputNoteRecord {
        let bridge = AccountId::from_hex("0xaa0000000000bb110000cc000000dd").unwrap();
        let attachments = NoteAttachments::default();
        let metadata = enriched.then(|| {
            NoteMetadata::new(
                PartialNoteMetadata::new(bridge, NoteType::Public),
                &attachments,
            )
        });
        let details = NoteDetails::new(
            NoteAssets::default(),
            NoteRecipient::new(
                Word::from([Felt::from(serial), Felt::ZERO, Felt::ZERO, Felt::ZERO]),
                miden_base_agglayer::B2AggNote::script(),
                NoteStorage::default(),
            ),
        );
        let state = if consumed {
            InputNoteState::ConsumedExternal(ConsumedExternalNoteState {
                nullifier_block_height: BlockNumber::from(1),
                consumer_account: enriched.then_some(bridge),
                consumed_tx_order: enriched.then_some(2),
                metadata,
            })
        } else {
            InputNoteState::Expected(ExpectedNoteState {
                metadata,
                after_block_num: BlockNumber::from(0),
                tag: None,
            })
        };
        InputNoteRecord::new(details, attachments, Some(1), state)
    }

    async fn fixture() -> (crate::miden_client::MidenClientLib, SqliteStore) {
        let client = crate::test_helpers::offline_miden_client_lib().await;
        // Test writes are sequential with client operations. Production never
        // opens another SDK store, and all index work uses the client's queue.
        let store = SqliteStore::new(client.store_identifier().into())
            .await
            .unwrap();
        (client, store)
    }

    #[tokio::test]
    async fn history_is_read_once_then_only_changes_and_explicit_retries() {
        let (mut client, store) = fixture().await;
        let history: Vec<_> = (0..15_000).map(|i| note(i, true, false)).collect();
        store.upsert_input_notes(&history).await.unwrap();
        let feed = ConsumedNoteFeed::default();
        let mut access = ClientAccess::Exclusive(&mut client);
        let cold = feed.load(&mut access, false, &[]).await.unwrap();
        assert_eq!(cold.notes.len(), 15_000);
        feed.commit(&cold, []);
        assert!(
            feed.load(&mut access, false, &[])
                .await
                .unwrap()
                .notes
                .is_empty()
        );
        let late = note(15_000, true, false);
        store
            .upsert_input_notes(std::slice::from_ref(&late))
            .await
            .unwrap();
        let changed = feed.load(&mut access, false, &[]).await.unwrap();
        assert_eq!(changed.notes, vec![late.clone()]);
        // Not acknowledging failed work must replay it.
        assert_eq!(
            feed.load(&mut access, false, &[]).await.unwrap().notes,
            changed.notes
        );
        feed.commit(&changed, [late.details_commitment().as_bytes()]);
        let retry = feed.load(&mut access, false, &[]).await.unwrap();
        assert_eq!(retry.notes, vec![late]);
        feed.commit(&retry, []);
        assert!(
            feed.load(&mut access, false, &[])
                .await
                .unwrap()
                .notes
                .is_empty()
        );
        // Re-registering an old already-consumed CLAIM must still find it.
        let explicit = feed
            .load(
                &mut access,
                false,
                &[history[0].details_commitment().as_bytes()],
            )
            .await
            .unwrap();
        assert_eq!(explicit.notes, vec![history[0].clone()]);
        // Registry changes and a fresh process deliberately rebuild inventory.
        assert_eq!(
            feed.load(&mut access, true, &[]).await.unwrap().notes.len(),
            15_001
        );
        assert_eq!(
            ConsumedNoteFeed::default()
                .load(&mut access, false, &[])
                .await
                .unwrap()
                .notes
                .len(),
            15_001
        );
    }

    #[tokio::test]
    async fn old_pending_note_consumption_and_late_metadata_are_not_lost() {
        let (mut client, store) = fixture().await;
        store
            .upsert_input_notes(&[note(1, false, false)])
            .await
            .unwrap();
        let feed = ConsumedNoteFeed::default();
        let mut access = ClientAccess::Exclusive(&mut client);
        let cold = feed.load(&mut access, false, &[]).await.unwrap();
        assert!(cold.notes.is_empty());
        feed.commit(&cold, []);
        let consumed = note(1, true, false);
        store
            .upsert_input_notes(std::slice::from_ref(&consumed))
            .await
            .unwrap();
        let batch = feed.load(&mut access, false, &[]).await.unwrap();
        assert_eq!(batch.notes, vec![consumed.clone()]);
        assert!(batch.notes[0].metadata().is_none());
        assert!(batch.notes[0].consumer_account().is_none());
        feed.commit(&batch, []);
        // An identical SDK upsert must not turn every warm poll into a scan.
        store.upsert_input_notes(&[consumed]).await.unwrap();
        assert!(
            feed.load(&mut access, false, &[])
                .await
                .unwrap()
                .notes
                .is_empty()
        );
        let enriched = note(1, true, true);
        store
            .upsert_input_notes(std::slice::from_ref(&enriched))
            .await
            .unwrap();
        let batch = feed.load(&mut access, false, &[]).await.unwrap();
        assert_eq!(batch.notes, vec![enriched]);
        feed.commit(&batch, []);
        assert!(
            feed.load(&mut access, false, &[])
                .await
                .unwrap()
                .notes
                .is_empty()
        );
    }

    #[tokio::test]
    async fn unsupported_schema_falls_back_without_breaking_sdk_writes() {
        let (mut client, store) = fixture().await;
        store
            .upsert_input_notes(&[note(1, true, false)])
            .await
            .unwrap();
        let conn = Connection::open(client.store_identifier()).unwrap();
        let feed = ConsumedNoteFeed::default();
        let mut access = ClientAccess::Exclusive(&mut client);
        let initial = feed.load(&mut access, false, &[]).await.unwrap();
        feed.commit(&initial, []);
        conn.execute_batch("ALTER TABLE input_notes ADD COLUMN future_observation BLOB;")
            .unwrap();
        let fallback = feed.load(&mut access, false, &[]).await.unwrap();
        assert_eq!(fallback.notes.len(), 1);
        assert!(fallback.mark.is_none());
        let triggers: i64 = conn.query_row("SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name LIKE 'agglayer_note_%_v1'", [], |row| row.get(0)).unwrap();
        assert_eq!(triggers, 0);
        store
            .upsert_input_notes(&[note(2, true, false)])
            .await
            .unwrap();
        feed.commit(&fallback, []);
        assert_eq!(
            feed.load(&mut access, false, &[])
                .await
                .unwrap()
                .notes
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn rollback_and_index_recreation_preserve_recovery() {
        let (mut client, store) = fixture().await;
        let consumed = note(1, true, false);
        store.upsert_input_notes(&[consumed]).await.unwrap();
        let path = client.store_identifier().to_owned();
        let feed = ConsumedNoteFeed::default();
        let mut access = ClientAccess::Exclusive(&mut client);
        let cold = feed.load(&mut access, false, &[]).await.unwrap();
        feed.commit(&cold, []);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("BEGIN; UPDATE input_notes SET created_at=created_at+1; ROLLBACK;")
            .unwrap();
        let unchanged = feed.load(&mut access, false, &[]).await.unwrap();
        assert!(unchanged.notes.is_empty());
        assert_eq!(unchanged.mark, cold.mark);
        // Losing an observer trigger forces a new epoch and a full recovery
        // read, including notes that predate the observer's reinstallation.
        conn.execute_batch("DROP TRIGGER agglayer_note_update_v1;")
            .unwrap();
        let rebuilt = feed.load(&mut access, false, &[]).await.unwrap();
        assert_eq!(rebuilt.notes.len(), 1);
        assert_ne!(
            rebuilt.mark.as_ref().unwrap().epoch,
            cold.mark.as_ref().unwrap().epoch
        );
        feed.commit(&rebuilt, []);
        assert!(
            feed.load(&mut access, false, &[])
                .await
                .unwrap()
                .notes
                .is_empty()
        );
        conn.execute_batch("DELETE FROM input_notes;").unwrap();
        let deleted = feed.load(&mut access, false, &[]).await.unwrap();
        assert!(deleted.notes.is_empty());
        assert!(deleted.mark.as_ref().unwrap().revision > rebuilt.mark.as_ref().unwrap().revision);
        feed.commit(&deleted, []);
        store
            .upsert_input_notes(&[note(1, true, false)])
            .await
            .unwrap();
        assert_eq!(
            feed.load(&mut access, false, &[])
                .await
                .unwrap()
                .notes
                .len(),
            1
        );
    }
}
