//! Reconciliation borrows the SDK only for concrete SDK operations. The live
//! path queues those operations; offline restore keeps its exclusive client.

use crate::miden_client::{ClientQueue, MidenClientLib};
use miden_client::account::Account;
use miden_client::note::NoteFile;
use miden_client::store::{InputNoteRecord, NoteFilter};
use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;
use std::future::Future;
use std::pin::Pin;

pub enum ClientAccess<'a> {
    Exclusive(&'a mut MidenClientLib),
    Queued(ClientQueue),
}

impl ClientAccess<'_> {
    pub(crate) fn exclusive(&mut self) -> Option<&mut MidenClientLib> {
        match self {
            Self::Exclusive(client) => Some(client),
            Self::Queued(_) => None,
        }
    }

    pub(crate) async fn call<T, F>(&mut self, operation: &'static str, call: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: for<'c> FnOnce(
            &'c mut MidenClientLib,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send + 'c>>,
        F: Send + 'static,
    {
        match self {
            Self::Exclusive(client) => call(client).await,
            Self::Queued(queue) => queue.call(operation, call).await,
        }
    }

    pub async fn get_input_notes(
        &mut self,
        filter: NoteFilter,
    ) -> anyhow::Result<Vec<InputNoteRecord>> {
        self.call("reconciliation::read_notes", move |client| {
            Box::pin(async move { Ok(client.get_input_notes(filter).await?) })
        })
        .await
    }

    pub async fn get_account(&mut self, id: AccountId) -> anyhow::Result<Option<Account>> {
        self.call("reconciliation::read_account", move |client| {
            Box::pin(async move { Ok(client.get_account(id).await?) })
        })
        .await
    }

    pub async fn get_sync_height(&mut self) -> anyhow::Result<BlockNumber> {
        self.call("reconciliation::read_height", |client| {
            Box::pin(async move { Ok(client.get_sync_height().await?) })
        })
        .await
    }

    pub async fn import_notes(&mut self, notes: &[NoteFile]) -> anyhow::Result<()> {
        let notes = notes.to_vec();
        self.call("reconciliation::import_notes", move |client| {
            Box::pin(async move {
                client.import_notes(&notes).await?;
                Ok(())
            })
        })
        .await
    }

    pub async fn import_account_by_id(&mut self, id: AccountId) -> anyhow::Result<()> {
        self.call("reconciliation::import_account", move |client| {
            Box::pin(async move {
                client.import_account_by_id(id).await?;
                Ok(())
            })
        })
        .await
    }
}
