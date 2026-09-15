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
