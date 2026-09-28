//! Share an in-progress authoritative read without reusing completed snapshots.

use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

type Outcome<T> = Result<Arc<T>, Arc<str>>;
type Receiver<T> = watch::Receiver<Option<Outcome<T>>>;

pub(crate) struct CoalescedRead<T> {
    current: Mutex<Option<Receiver<T>>>,
}

impl<T> Default for CoalescedRead<T> {
    fn default() -> Self {
        Self {
            current: Mutex::new(None),
        }
    }
}

pub(crate) struct ReadPublisher<T>(watch::Sender<Option<Outcome<T>>>);

impl<T> Clone for ReadPublisher<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> ReadPublisher<T> {
    /// Publish while still holding the serialized source. A later reader must
    /// start a new read even if earlier callers have not consumed this result.
    pub(crate) fn finish(&self, result: anyhow::Result<T>) {
        let mut result = Some(
            result
                .map(Arc::new)
                .map_err(|e| Arc::from(format!("{e:#}"))),
        );
        self.0.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = result.take();
            true
        });
    }
}

impl<T: Send + Sync + 'static> CoalescedRead<T> {
    pub(crate) async fn read<F, Fut>(&self, load: F) -> anyhow::Result<Arc<T>>
    where
        F: FnOnce(ReadPublisher<T>) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let (mut receiver, publisher) = {
            let mut current = self.current.lock().expect("coalesced read mutex poisoned");
            match current.as_ref() {
                Some(receiver) if receiver.borrow().is_none() && receiver.has_changed().is_ok() => {
                    (receiver.clone(), None)
                }
                _ => {
                    let (sender, receiver) = watch::channel(None);
                    *current = Some(receiver.clone());
                    (receiver, Some(ReadPublisher(sender)))
                }
            }
        };
        if let Some(publisher) = publisher {
            // A cancelled RPC must not cancel the read shared by other callers.
            // This task only reads; transaction submission never uses this path.
            tokio::spawn(async move {
                let error = match load(publisher.clone()).await {
                    Err(error) => error,
                    Ok(()) => anyhow::anyhow!("authoritative read returned no snapshot"),
                };
                // Covers enqueue/channel errors. Never replaces a result that
                // was already published inside the serialized source.
                publisher.finish(Err(error));
            });
        }
        let outcome = receiver
            .wait_for(|value| value.is_some())
            .await
            .map_err(|_| {
                anyhow::anyhow!("authoritative read stopped before publishing a snapshot")
            })?
            .as_ref()
            .expect("wait_for required a result")
            .clone();
        outcome.map_err(|error| anyhow::anyhow!(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn overlapping_reads_share_one_load_even_when_first_caller_is_cancelled() {
        let reads = Arc::new(CoalescedRead::default());
        let loads = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let first = tokio::spawn({
            let reads = reads.clone();
            let loads = loads.clone();
            async move {
                reads
                    .read(move |publisher| async move {
                        loads.fetch_add(1, Ordering::SeqCst);
                        started_tx.send(()).unwrap();
                        release_rx.await.unwrap();
                        publisher.finish(Ok(42));
                        Ok(())
                    })
                    .await
            }
        });
        started_rx.await.unwrap();
        // Poll each waiter into the shared read before releasing its source.
        let mut waiters = Vec::new();
        for _ in 0..64 {
            let read = reads.read(|_| async { panic!("duplicate source read") });
            let mut read = Box::pin(read);
            assert!(matches!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(read.as_mut().poll(cx))).await,
                std::task::Poll::Pending
            ));
            waiters.push(read);
        }
        first.abort();
        release_tx.send(()).unwrap();
        for waiter in waiters {
            assert_eq!(*waiter.await.unwrap(), 42);
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn publication_ends_sharing_before_loader_returns() {
        let reads = Arc::new(CoalescedRead::default());
        let (release_tx, release_rx) = oneshot::channel();
        let first = reads
            .read(move |publisher| async move {
                publisher.finish(Ok(1));
                // Models the client processing a subsequent write before the
                // original request's outer response future has been scheduled.
                release_rx.await.unwrap();
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(*first, 1);
        let second = reads
            .read(|publisher| async move {
                publisher.finish(Ok(2));
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(*second, 2, "a completed account read must never be cached");
        release_tx.send(()).unwrap();
    }

    #[tokio::test]
    async fn failed_or_abandoned_loader_fails_closed_and_next_read_retries() {
        let reads = CoalescedRead::<u32>::default();
        let error = reads
            .read(|_| async { anyhow::bail!("client unavailable") })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("client unavailable"));
        let error = reads.read(|_| async { Ok(()) }).await.unwrap_err();
        assert!(error.to_string().contains("no snapshot"));
        let value = reads
            .read(|publisher| async move {
                publisher.finish(Ok(3));
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(*value, 3);
    }
}
