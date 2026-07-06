//! Empirical probes against a Miden node.
//!
//! Mode 1 (legacy, positional args): do SyncNotes / GetNotesById return
//! already-CONSUMED notes?
//!
//! ```text
//! note_probe <url> <note_id_hex> <from> <to>
//! ```
//!
//! Mode 2 (`--bench-sweep`): measure the reconciler's window-fetch pipeline
//! (`sync_notes` + candidate-id extraction, NO imports) over a block range —
//! sequential (the historical one-window-at-a-time sweep) vs concurrent
//! (`RECONCILE_CONCURRENCY`-style in-flight windows), plus a chunk-size probe.
//! This is the evidence harness for the budgeted concurrent catch-up in
//! `synthetic_projector::reconcile_notes`.
//!
//! ```text
//! note_probe --bench-sweep --node https://rpc.testnet.miden.io \
//!     [--from 1] [--blocks 10000] [--cap-secs 300] \
//!     [--chunk 200] [--concurrency 8] [--probe-chunks 200,1000,2000]
//! ```
use miden_client::rpc::NodeRpcClient;
use miden_protocol::note::{NoteId, NoteTag};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

/// One reconciler window fetch: `sync_notes` over `[from, to]` + candidate-id
/// extraction (the exact pre-import per-window work). Returns the note count.
async fn fetch_window(rpc: &Arc<dyn NodeRpcClient>, from: u64, to: u64) -> anyhow::Result<usize> {
    let tags: BTreeSet<NoteTag> = BTreeSet::from([NoteTag::from(0u32)]);
    let blocks = rpc
        .sync_notes((from as u32).into(), (to as u32).into(), &tags)
        .await
        .map_err(|e| anyhow::anyhow!("sync_notes({from}..{to}): {e}"))?;
    let candidates: Vec<NoteId> = blocks
        .iter()
        .flat_map(|b| b.notes.keys().copied())
        .collect();
    Ok(candidates.len())
}

fn windows(from: u64, to: u64, chunk: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut f = from;
    while f <= to {
        let t = (f + chunk - 1).min(to);
        out.push((f, t));
        f = t + 1;
    }
    out
}

struct RunReport {
    blocks: u64,
    elapsed: Duration,
    notes: usize,
}

impl RunReport {
    fn rate(&self) -> f64 {
        self.blocks as f64 / self.elapsed.as_secs_f64().max(1e-9)
    }
}

/// Sequential pipeline: one window in flight at a time — the OLD sweep's fetch
/// pattern, minus its 5s-per-window tick cap (raw RPC-bound throughput).
async fn run_sequential(
    rpc: &Arc<dyn NodeRpcClient>,
    wins: &[(u64, u64)],
    cap: Duration,
) -> anyhow::Result<RunReport> {
    let start = Instant::now();
    let (mut blocks, mut notes) = (0u64, 0usize);
    for &(f, t) in wins {
        if start.elapsed() >= cap {
            break;
        }
        notes += fetch_window(rpc, f, t).await?;
        blocks += t - f + 1;
    }
    Ok(RunReport {
        blocks,
        elapsed: start.elapsed(),
        notes,
    })
}

/// Concurrent pipeline: up to `concurrency` windows in flight (sliding refill,
/// buffer_unordered-style) — the NEW reconciler fetch stage.
async fn run_concurrent(
    rpc: &Arc<dyn NodeRpcClient>,
    wins: &[(u64, u64)],
    concurrency: usize,
    cap: Duration,
) -> anyhow::Result<RunReport> {
    let start = Instant::now();
    let (mut blocks, mut notes) = (0u64, 0usize);
    let mut next = 0usize;
    let mut set = tokio::task::JoinSet::new();
    let spawn = |set: &mut tokio::task::JoinSet<anyhow::Result<(usize, u64)>>, i: usize| {
        let (f, t) = wins[i];
        let rpc = Arc::clone(rpc);
        set.spawn(async move { fetch_window(&rpc, f, t).await.map(|n| (n, t - f + 1)) });
    };
    while next < wins.len() && set.len() < concurrency {
        spawn(&mut set, next);
        next += 1;
    }
    while let Some(joined) = set.join_next().await {
        let (n, b) = joined??;
        notes += n;
        blocks += b;
        if next < wins.len() && start.elapsed() < cap {
            spawn(&mut set, next);
            next += 1;
        }
    }
    Ok(RunReport {
        blocks,
        elapsed: start.elapsed(),
        notes,
    })
}

async fn bench_sweep(args: Vec<String>) -> anyhow::Result<()> {
    let node = arg_value(&args, "--node")
        .ok_or_else(|| anyhow::anyhow!("--bench-sweep requires --node <url>"))?;
    let from: u64 = arg_value(&args, "--from").map_or(Ok(1), |v| v.parse())?;
    let blocks: u64 = arg_value(&args, "--blocks").map_or(Ok(10_000), |v| v.parse())?;
    let cap = Duration::from_secs(arg_value(&args, "--cap-secs").map_or(Ok(300), |v| v.parse())?);
    let chunk: u64 = arg_value(&args, "--chunk").map_or(Ok(200), |v| v.parse())?;
    let concurrency: usize = arg_value(&args, "--concurrency").map_or(Ok(8), |v| v.parse())?;
    let probe_chunks: Vec<u64> = arg_value(&args, "--probe-chunks")
        .unwrap_or_else(|| "200,1000,2000".into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    let ep = miden_agglayer_service::miden_client::parse_node_url(&node)?;
    let rpc = miden_agglayer_service::miden_client::build_rpc_client(&ep, 30_000, None);

    let (tip_header, _) = rpc
        .get_block_header_by_number(None, false)
        .await
        .map_err(|e| anyhow::anyhow!("get_block_header_by_number(tip): {e}"))?;
    let tip = tip_header.block_num().as_u64();
    let to = (from + blocks - 1).min(tip);
    println!(
        "node tip: {tip}; benchmarking blocks {from}..{to} (cap {}s per pipeline)",
        cap.as_secs()
    );

    // Chunk-size probe: does the node accept larger sync_notes spans, and are
    // they faster per block? One call per size.
    println!("\n== chunk-size probe (single window per size) ==");
    for &c in &probe_chunks {
        let t = (from + c - 1).min(tip);
        let started = Instant::now();
        match fetch_window(&rpc, from, t).await {
            Ok(n) => println!(
                "  span {c:>5}: OK  {:>8.1} ms  ({n} notes)  {:>8.1} blocks/s single-stream",
                started.elapsed().as_secs_f64() * 1e3,
                (t - from + 1) as f64 / started.elapsed().as_secs_f64()
            ),
            Err(e) => println!(
                "  span {c:>5}: REJECTED/ERR after {:?}: {e}",
                started.elapsed()
            ),
        }
    }

    let wins = windows(from, to, chunk);
    println!(
        "\n== sequential pipeline (chunk {chunk}, 1 in flight) — OLD sweep minus its 5s tick cap =="
    );
    let seq = run_sequential(&rpc, &wins, cap).await?;
    println!(
        "  blocks {:>6}  wall {:>7.1}s  {:>8.1} blocks/s  ({} notes seen)",
        seq.blocks,
        seq.elapsed.as_secs_f64(),
        seq.rate(),
        seq.notes
    );

    println!(
        "\n== concurrent pipeline (chunk {chunk}, {concurrency} in flight) — NEW fetch stage =="
    );
    let conc = run_concurrent(&rpc, &wins, concurrency, cap).await?;
    println!(
        "  blocks {:>6}  wall {:>7.1}s  {:>8.1} blocks/s  ({} notes seen)",
        conc.blocks,
        conc.elapsed.as_secs_f64(),
        conc.rate(),
        conc.notes
    );

    println!("\n== summary ==");
    println!("  raw pipeline speedup: {:.1}x", conc.rate() / seq.rate());
    // Old effective prod rate: exactly ONE chunk-block window per ~5s sync
    // tick, regardless of RPC speed.
    let old_effective = (200.0 / 5.0f64).min(seq.rate());
    // New effective prod rate: the catch-up loop runs for RECONCILE_TICK_BUDGET_MS
    // (default 2000ms) of every ~5s tick at the concurrent pipeline rate.
    let new_effective = conc.rate() * (2.0 / 5.0);
    println!(
        "  old effective prod rate (1x200 window / 5s tick):     {old_effective:>8.1} blocks/s"
    );
    println!(
        "  new effective prod rate (2s budget of every 5s tick): {new_effective:>8.1} blocks/s  ({:.1}x end-to-end)",
        new_effective / old_effective
    );
    println!(
        "  full-history sweep of {tip} blocks: old {:>6.2} h -> new {:>6.2} h",
        tip as f64 / old_effective / 3600.0,
        tip as f64 / new_effective / 3600.0
    );
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--bench-sweep") {
        return bench_sweep(args).await;
    }
    let mut a = args.into_iter();
    let (url, id_hex, from, to) = (
        a.next().unwrap(),
        a.next().unwrap(),
        a.next().unwrap().parse::<u32>()?,
        a.next().unwrap().parse::<u32>()?,
    );
    let ep = miden_agglayer_service::miden_client::parse_node_url(&url)?;
    let rpc = miden_agglayer_service::miden_client::build_rpc_client(&ep, 10_000, None);
    let id = NoteId::try_from_hex(&id_hex)?;
    let tags: BTreeSet<NoteTag> = BTreeSet::from([NoteTag::from(0u32)]);
    let blocks = rpc.sync_notes(from.into(), to.into(), &tags).await?;
    let listed = blocks.iter().any(|b| b.notes.contains_key(&id));
    let total: usize = blocks.iter().map(|b| b.notes.len()).sum();
    println!(
        "sync_notes {from}..{to}: blocks={} notes={} target_listed={listed}",
        blocks.len(),
        total
    );
    match rpc.get_notes_by_id(&[id]).await {
        Ok(v) => println!("get_notes_by_id: returned {} note(s)", v.len()),
        Err(e) => println!("get_notes_by_id ERR: {e}"),
    }
    Ok(())
}
