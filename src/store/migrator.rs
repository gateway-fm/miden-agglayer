//! In-process Postgres migrator. Runs at proxy startup before `PgStore`
//! opens the connection pool.
//!
//! ## Why
//!
//! Until this module existed, schema migrations were applied by a
//! separate `agglayer-migrate` one-shot docker-compose service that
//! hardcoded the migration list in its `command:` block. New
//! migrations required editing both the SQL file AND the compose
//! file. That footgun was the proximate cause of `005_l1_indexer_cursor.sql`
//! shipping unwired in an earlier iteration of this branch. Production
//! presumably had the same shape (init container or k8s Job with a
//! hardcoded list) — so the proxy could ship a migration its DB
//! never received.
//!
//! ## Design
//!
//! - Migrations are embedded into the binary via `include_str!`. The
//!   list is alphabetically ordered (`001_…`, `002_…`, etc.) so
//!   downstream tools and operators can grep the binary for them.
//! - A `schema_migrations(name, checksum, applied_at)` tracking table
//!   is created on first run if absent.
//! - On every startup the migrator:
//!     1. Acquires a SERIALIZABLE-level Postgres advisory lock so two
//!        pods racing on startup don't double-apply.
//!     2. For each migration in order: if absent → apply the file +
//!        INSERT row + commit; if present with matching checksum →
//!        skip; if present with MISMATCHING checksum → return Err
//!        (loud: a previously-applied migration was edited, which is
//!        a deployment bug, not something we can paper over).
//!     3. Releases the advisory lock.
//! - Idempotent: re-running is safe. Empty DB or fully-migrated DB
//!   both converge.
//!
//! ## Caller contract
//!
//! `main.rs` calls `run_migrations(&db_url).await?` immediately after
//! parsing `--database-url` and before constructing `PgStore`. Errors
//! propagate up and abort startup. The InMemoryStore code path (when
//! `--database-url` is unset) does not need migrations.

use anyhow::{Context, Result};
use sha3::{Digest, Keccak256};
use tokio_postgres::{Client, NoTls};

/// Compile-time embedded list of `(filename, sql)`. KEEP THIS IN
/// LEXICOGRAPHIC ORDER. New migrations are added at the end.
const MIGRATIONS: &[(&str, &str)] = &[
    (
        "001_initial.sql",
        include_str!("../../migrations/001_initial.sql"),
    ),
    (
        "002_faucet_registry.sql",
        include_str!("../../migrations/002_faucet_registry.sql"),
    ),
    (
        "003_unclaimable_claims.sql",
        include_str!("../../migrations/003_unclaimable_claims.sql"),
    ),
    (
        "004_claim_watcher.sql",
        include_str!("../../migrations/004_claim_watcher.sql"),
    ),
    (
        "005_l1_indexer_cursor.sql",
        include_str!("../../migrations/005_l1_indexer_cursor.sql"),
    ),
    (
        "006_unbridgeable_bridge_outs.sql",
        include_str!("../../migrations/006_unbridgeable_bridge_outs.sql"),
    ),
    (
        "007_monitor_state_persistence.sql",
        include_str!("../../migrations/007_monitor_state_persistence.sql"),
    ),
    (
        "008_faucet_metadata.sql",
        include_str!("../../migrations/008_faucet_metadata.sql"),
    ),
    (
        "009_synthetic_projector.sql",
        include_str!("../../migrations/009_synthetic_projector.sql"),
    ),
    (
        "010_reconcile_cursor.sql",
        include_str!("../../migrations/010_reconcile_cursor.sql"),
    ),
    (
        "011_nonce_reservations.sql",
        include_str!("../../migrations/011_nonce_reservations.sql"),
    ),
];

/// Postgres advisory-lock key. Arbitrary 64-bit int; just needs to be
/// stable across all proxy instances so they serialise.
const ADVISORY_LOCK_KEY: i64 = 0x1729_2026_0518_0001_u64 as i64;

/// Result of one migrator run, suitable for structured logging.
#[derive(Debug, Clone, Default)]
pub struct MigrationReport {
    pub applied: Vec<String>,
    pub already_present: Vec<String>,
}

/// Connect to `db_url`, apply any pending migrations from the embedded
/// list, return a report. Errors abort proxy startup.
pub async fn run_migrations(db_url: &str) -> Result<MigrationReport> {
    let (client, connection) = tokio_postgres::connect(db_url, NoTls)
        .await
        .context("connecting to Postgres for migrations")?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::error!(error = %e, "migrator: postgres connection task ended with error");
        }
    });
    let result = run_migrations_with_client(&client).await;
    drop(client);
    result
}

/// Inner: takes an already-connected client so tests can pass a
/// pg_temporary or testcontainer client.
async fn run_migrations_with_client(client: &Client) -> Result<MigrationReport> {
    ensure_schema_migrations_table(client).await?;

    // Take the advisory lock; held for the rest of this connection's
    // lifetime (released on drop).
    client
        .execute(
            &format!("SELECT pg_advisory_lock({ADVISORY_LOCK_KEY})"),
            &[],
        )
        .await
        .context("acquiring advisory lock")?;

    let mut report = MigrationReport::default();

    for (name, sql) in MIGRATIONS {
        let checksum = compute_checksum(sql);
        let row = client
            .query_opt(
                "SELECT checksum FROM schema_migrations WHERE name = $1",
                &[name],
            )
            .await
            .with_context(|| format!("looking up migration {name}"))?;

        match row {
            None => {
                // Apply
                tracing::info!(migration = %name, "applying migration");
                client
                    .batch_execute(sql)
                    .await
                    .with_context(|| format!("applying migration {name}"))?;
                client
                    .execute(
                        "INSERT INTO schema_migrations (name, checksum) VALUES ($1, $2)",
                        &[name, &checksum.as_str()],
                    )
                    .await
                    .with_context(|| format!("recording applied migration {name}"))?;
                report.applied.push((*name).into());
            }
            Some(existing) => {
                let recorded: &str = existing.get(0);
                if recorded != checksum {
                    anyhow::bail!(
                        "migration {name} previously applied with checksum {recorded} \
                         but the file currently embedded checksums to {checksum}. \
                         A previously-applied migration was modified — this is a \
                         deployment bug and not something the proxy will paper over. \
                         Either revert the edit, or rename to a new migration file \
                         that supersedes it."
                    );
                }
                report.already_present.push((*name).into());
            }
        }
    }

    // Release the advisory lock explicitly (it'd release on disconnect
    // anyway, but being explicit makes the intent obvious).
    client
        .execute(
            &format!("SELECT pg_advisory_unlock({ADVISORY_LOCK_KEY})"),
            &[],
        )
        .await
        .context("releasing advisory lock")?;

    tracing::info!(
        applied = report.applied.len(),
        already_present = report.already_present.len(),
        applied_names = ?report.applied,
        "migration run complete"
    );
    Ok(report)
}

async fn ensure_schema_migrations_table(client: &Client) -> Result<()> {
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                name        TEXT PRIMARY KEY,
                checksum    TEXT NOT NULL,
                applied_at  TIMESTAMPTZ NOT NULL DEFAULT now()
            );",
        )
        .await
        .context("creating schema_migrations table")
}

fn compute_checksum(sql: &str) -> String {
    let mut hasher = Keccak256::new();
    hasher.update(sql.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_list_is_lexicographically_sorted() {
        let names: Vec<&str> = MIGRATIONS.iter().map(|(n, _)| *n).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(
            names, sorted,
            "MIGRATIONS list must be in lexicographic order"
        );
    }

    #[test]
    fn migration_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for (name, _) in MIGRATIONS {
            assert!(seen.insert(*name), "duplicate migration name: {name}");
        }
    }

    #[test]
    fn checksum_is_deterministic_and_changes_with_content() {
        let c1 = compute_checksum("CREATE TABLE x();");
        let c2 = compute_checksum("CREATE TABLE x();");
        let c3 = compute_checksum("CREATE TABLE y();");
        assert_eq!(c1, c2);
        assert_ne!(c1, c3);
        assert_eq!(c1.len(), 64); // sha256 hex
    }

    /// Live-Postgres integration test. Skipped unless DATABASE_URL is set,
    /// which `make e2e-up` provides via the agglayer-postgres container.
    ///
    /// Run with `DATABASE_URL=host=127.0.0.1 port=5434 user=agglayer password=agglayer dbname=agglayer_store cargo test --features postgres migrator::tests::live -- --nocapture`.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn live_run_migrations_is_idempotent() {
        let db_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to run this test");
        let r1 = run_migrations(&db_url).await.expect("first run");
        let r2 = run_migrations(&db_url).await.expect("second run");
        // Second run must apply nothing.
        assert!(
            r2.applied.is_empty(),
            "second run applied: {:?}",
            r2.applied
        );
        assert_eq!(
            r2.already_present.len(),
            MIGRATIONS.len(),
            "second run did not see all migrations as already-present"
        );
        // First run applied at least 0; if the DB was fresh it applied all.
        assert!(r1.applied.len() + r1.already_present.len() == MIGRATIONS.len());
    }
}
