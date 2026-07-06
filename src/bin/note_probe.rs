//! Empirical probe: do SyncNotes / GetNotesById return already-CONSUMED notes?
use miden_protocol::note::{NoteId, NoteTag};
use std::collections::BTreeSet;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut a = std::env::args().skip(1);
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
