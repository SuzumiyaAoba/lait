//! A process-wide, unbounded `String → Arc<V>` cache for expensive-to-build
//! immutable values — `template::TEMPLATE_CACHE` (compiled handlebars
//! templates) and `jq::FILTER_CACHE` (compiled jq filters) used to spell the
//! same `LazyLock<Mutex<HashMap<String, Arc<_>>>>` plus
//! lock/lookup/compile-outside-the-lock/insert sequence by hand, including
//! the same poison-recovery policy.
//!
//! Deliberately unbounded: every caller's key space is fixed by the
//! workflow/config files the invocation loaded (`lait lint <DIR>` is the
//! outer bound — every workflow file in a directory tree — and still only
//! grows with the distinct sources on disk, and the process exits when it
//! finishes). Keys are always the full source text, so different callers'
//! entries can never alias one another's value type — each `SyncCache<V>`
//! static is its own namespace.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Result;

/// See this module's doc comment for the key/unboundedness contract.
pub(crate) struct SyncCache<V> {
    entries: Mutex<HashMap<String, Arc<V>>>,
}

impl<V> SyncCache<V> {
    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Whether `key` already has an entry — `jq::validate_filter_source`
    /// uses this to short-circuit its scan: a source text only ever enters
    /// the cache *after* that same validation passed, so presence is sound
    /// evidence of validity.
    pub(crate) fn contains(&self, key: &str) -> bool {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(key)
    }

    /// Returns `key`'s cached value, or `compile`s, caches, and returns it.
    /// `compile` runs *outside* the lock — the lock only ever covers a map
    /// lookup/insert, so compile time never serializes unrelated callers —
    /// which means two callers racing the same key may both compile and the
    /// last insert wins. Both sides produce an equivalent `V` for the same
    /// key, so the lost compile is wasted work, never wrong data.
    ///
    /// A poisoned lock is recovered with `PoisonError::into_inner`: the map
    /// is append-only and each value is immutable once stored, so a panic
    /// mid-`insert` can at most leave a fully-formed map.
    pub(crate) fn get_or_init(
        &self,
        key: &str,
        compile: impl FnOnce(&str) -> Result<V>,
    ) -> Result<Arc<V>> {
        if let Some(cached) = self
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
        {
            return Ok(Arc::clone(cached));
        }

        let value = Arc::new(compile(key)?);
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.to_owned(), Arc::clone(&value));
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::SyncCache;
    use std::sync::Arc;

    #[test]
    fn contains_is_false_until_the_key_has_actually_been_inserted() {
        let cache: SyncCache<u32> = SyncCache::new();
        assert!(!cache.contains("a"));
        cache.get_or_init("a", |_| Ok(1)).unwrap();
        assert!(cache.contains("a"));
    }

    #[test]
    fn a_cache_hit_returns_the_same_arc_without_recompiling() {
        let cache: SyncCache<u32> = SyncCache::new();
        let compiles = std::sync::atomic::AtomicU32::new(0);
        let compile = |_: &str| {
            compiles.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(42)
        };

        let first = cache.get_or_init("a", compile).unwrap();
        let second = cache.get_or_init("a", compile).unwrap();

        assert!(
            Arc::ptr_eq(&first, &second),
            "a cache hit must return the same Arc, not an equal-but-distinct clone"
        );
        assert_eq!(compiles.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// `compile` must run *outside* the lock — proven here by reentrancy,
    /// not timing: from inside key `"a"`'s own `compile` closure, look up a
    /// *different* key (`"b"`) on the same cache. If `get_or_init` held its
    /// lock across the `compile` call, this reentrant `contains`/`get_or_init`
    /// would deadlock on the same `Mutex`; since it doesn't, the lock was
    /// already released before `compile` ran.
    #[test]
    fn compile_runs_outside_the_lock() {
        let cache: SyncCache<u32> = SyncCache::new();
        let value = cache
            .get_or_init("a", |_| {
                assert!(!cache.contains("b"));
                let inner = cache.get_or_init("b", |_| Ok(2)).unwrap();
                Ok(*inner + 1)
            })
            .unwrap();
        assert_eq!(*value, 3);
        assert!(cache.contains("b"));
    }

    #[test]
    fn get_or_init_propagates_a_compile_error_without_caching_it() {
        let cache: SyncCache<u32> = SyncCache::new();
        let error = cache
            .get_or_init("a", |_| anyhow::bail!("compile failed"))
            .unwrap_err();
        assert!(error.to_string().contains("compile failed"));
        assert!(
            !cache.contains("a"),
            "a failed compile must not leave a cache entry behind"
        );
    }

    /// The map is append-only and every value is immutable once stored, so a
    /// panic while another thread holds the lock can at most leave a
    /// fully-formed map behind — recovering via `PoisonError::into_inner`
    /// (rather than propagating the poison) must still see that prior entry.
    ///
    /// This needs direct access to the private `entries` field to force a
    /// poison deterministically (locking it and panicking while held), which
    /// only this inline `#[cfg(test)] mod tests` can do — moving this test
    /// to `tests/` later would lose that access entirely.
    #[test]
    fn a_poisoned_lock_recovers_and_keeps_the_entry_inserted_before_the_panic() {
        let cache: Arc<SyncCache<u32>> = Arc::new(SyncCache::new());
        cache.get_or_init("a", |_| Ok(1)).unwrap();

        let poisoning = Arc::clone(&cache);
        let joined = std::thread::spawn(move || {
            let _guard = poisoning.entries.lock().unwrap();
            panic!("deliberately poisoning the lock for the test above");
        })
        .join();
        assert!(joined.is_err(), "the spawned thread must have panicked");

        // The pre-existing entry survives poison recovery, and the cache
        // keeps working for new keys afterward.
        assert!(cache.contains("a"));
        let value = cache.get_or_init("b", |_| Ok(2)).unwrap();
        assert_eq!(*value, 2);
    }
}
