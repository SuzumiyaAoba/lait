use std::{
    collections::HashMap,
    future::Future,
    hash::Hash,
    sync::{Arc, Mutex},
};

use anyhow::{Result, anyhow};
use tokio::sync::OnceCell;

use crate::{async_io, error::Interrupted};

/// A lazily initialized, per-key asynchronous cache.
///
/// The map lock is held only while looking up or inserting a key's cell. The
/// value initializer belongs to [`OnceCell`], so concurrent callers for the
/// same key share one in-flight load and callers for different keys do not
/// serialize their work on the map lock.
pub(crate) struct AsyncCache<K, V> {
    entries: Mutex<HashMap<K, Arc<OnceCell<Arc<V>>>>>,
}

impl<K, V> Default for AsyncCache<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> AsyncCache<K, V> {
    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl<K, V> AsyncCache<K, V>
where
    K: Eq + Hash,
{
    /// Returns the per-key cell used to initialize and retrieve a cached
    /// value. The returned cell can safely be awaited after this method
    /// releases the map lock.
    fn cell(&self, key: K) -> Arc<OnceCell<Arc<V>>> {
        Arc::clone(
            self.entries
                .lock()
                .expect("async cache lock should not be poisoned")
                .entry(key)
                .or_insert_with(|| Arc::new(OnceCell::new())),
        )
    }

    /// Initializes `key` once and waits for the value while honoring this
    /// caller's cancellation token. A cancelled initializer is dropped by
    /// [`OnceCell`], leaving the key retryable for a later caller; a caller
    /// that only waits on another task can cancel independently as well.
    pub(crate) async fn get_or_try_init<F, Fut>(
        &self,
        key: K,
        cancellation: Option<tokio_util::sync::CancellationToken>,
        init: F,
        cancellation_message: &'static str,
    ) -> Result<Arc<V>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Arc<V>>>,
    {
        match async_io::await_cancellation(self.cell(key).get_or_try_init(init), cancellation).await
        {
            async_io::CancellationResult::Completed(result) => Ok(Arc::clone(result?)),
            async_io::CancellationResult::Cancelled => {
                Err(anyhow!(Interrupted::cancelled(cancellation_message)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AsyncCache;
    use std::sync::Arc;

    #[tokio::test]
    async fn returns_the_same_cell_for_the_same_key() {
        let cache = AsyncCache::<String, String>::new();
        let first = cache.cell("key".to_owned());
        let second = cache.cell("key".to_owned());

        let value = first
            .get_or_init(|| async { Arc::new("value".to_owned()) })
            .await;
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(value.as_str(), "value");
        assert_eq!(
            second.get().map(Arc::as_ref).map(String::as_str),
            Some("value")
        );
    }

    #[tokio::test]
    async fn uses_different_cells_for_different_keys() {
        let cache = AsyncCache::<String, String>::new();
        let first = cache.cell("first".to_owned());
        let second = cache.cell("second".to_owned());

        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn returns_a_typed_error_when_the_waiter_is_already_cancelled() {
        let cache = AsyncCache::<String, String>::new();
        let cancellation = tokio_util::sync::CancellationToken::new();
        cancellation.cancel();

        let error = cache
            .get_or_try_init(
                "key".to_owned(),
                Some(cancellation),
                || async { Ok(Arc::new("value".to_owned())) },
                "cache initialization was cancelled",
            )
            .await
            .unwrap_err();

        assert!(
            error
                .chain()
                .any(|cause| cause.is::<crate::error::Interrupted>())
        );
    }
}
