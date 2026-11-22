//! Crate metadata for the language server.
//!
//! Versions and features come from the registry index through cargo. Search
//! results, readmes and repository links come from crates.io. Every lookup
//! runs on a blocking thread and reads the cargo config of the user.

use std::{
    collections::HashMap,
    hash::Hash,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use cargo::{GlobalContext, util::CargoResult};
pub use cargo::workspace::Summary;
pub use crates_io::Crate;
use tokio::sync::{Mutex, OnceCell};

pub mod api;

const CRATES_IO_API: &str = "https://crates.io";
const CRATES_IO_READMES: &str = "https://static.crates.io/readmes";

/// The state of one cached lookup.
pub enum CacheItemOut<V> {
    /// No fetch has started for the key.
    NotStarted,
    /// A fetch is running.
    Pending,
    /// The fetch failed with this message.
    Error(String),
    /// The fetch returned this value.
    Ready(V),
}

type Entry<V> = Arc<OnceCell<Result<V, String>>>;

/// Runs one blocking fetch per key and keeps the result, an error included.
/// Concurrent callers for one key share a single fetch.
#[derive(Default)]
struct Cache<K, V>(Mutex<HashMap<K, Entry<V>>>);

impl<K: Eq + Hash, V: Clone + Send + 'static> Cache<K, V> {
    async fn peek(&self, key: &K) -> CacheItemOut<V> {
        let state = self.0.lock().await.get(key).map(|cell| cell.get().cloned());
        match state {
            None => CacheItemOut::NotStarted,
            Some(None) => CacheItemOut::Pending,
            Some(Some(Ok(value))) => CacheItemOut::Ready(value),
            Some(Some(Err(e))) => CacheItemOut::Error(e),
        }
    }

    async fn get(
        &self,
        key: K,
        fetch: impl FnOnce() -> CargoResult<V> + Send + 'static,
    ) -> Result<V, String> {
        let cell = self.0.lock().await.entry(key).or_default().clone();
        cell.get_or_init(|| async move {
            match tokio::task::spawn_blocking(fetch).await {
                Ok(result) => result.map_err(|e| format!("{e:#}")),
                Err(e) => Err(e.to_string()),
            }
        })
        .await
        .clone()
    }

    /// Like [`Self::get`], but drops an error so that the next call fetches
    /// again.
    async fn get_or_retry(
        &self,
        key: K,
        fetch: impl FnOnce() -> CargoResult<V> + Send + 'static,
    ) -> Result<V, String>
    where
        K: Clone,
    {
        let result = self.get(key.clone(), fetch).await;
        if result.is_err() {
            self.0.lock().await.remove(&key);
        }
        result
    }

    async fn clear(&self) {
        self.0.lock().await.clear();
    }
}

/// Fetches and caches crate data for the language server.
///
/// Every method is safe to call from many tasks at once. A blocking fetch runs
/// on the tokio blocking pool, so the methods need a tokio runtime.
pub struct InfoProvider {
    per_page: AtomicU32,
    readmes: Cache<(String, String), String>,
    /// Keyed by registry name, where `None` is crates.io, and crate name.
    summaries: Cache<(Option<String>, String), Vec<Summary>>,
    searches: Cache<String, Arc<[Crate]>>,
}

impl InfoProvider {
    /// `per_page` is the most results that one search returns.
    pub fn new(per_page: u32) -> Self {
        Self {
            per_page: AtomicU32::new(per_page),
            readmes: Cache::default(),
            summaries: Cache::default(),
            searches: Cache::default(),
        }
    }

    /// Sets the search result limit and drops the cached searches.
    pub async fn set_per_page(&self, per_page: u32) {
        self.per_page.store(per_page, Ordering::Relaxed);
        self.searches.clear().await;
    }

    /// Returns the cached result of [`Self::get_info`] without starting it.
    pub async fn get_info_cache(&self, registry: Option<&str>, name: &str) -> CacheItemOut<Vec<Summary>> {
        self.summaries
            .peek(&(registry.map(ToOwned::to_owned), name.to_owned()))
            .await
    }

    /// Returns every non-yanked version of `name` in ascending version order.
    ///
    /// `registry` names a registry from the cargo config of the user, and
    /// `None` means crates.io. A crate that the registry does not have gives
    /// an empty list. The first call for a crate queries the registry, and
    /// later calls return the same result, an error included.
    pub async fn get_info(&self, registry: Option<&str>, name: &str) -> Result<Vec<Summary>, String> {
        let key = (registry.map(ToOwned::to_owned), name.to_owned());
        let (registry, name) = key.clone();
        self.summaries
            .get(key, move || {
                api::query_index(&GlobalContext::default()?, registry.as_deref(), &name)
            })
            .await
    }

    /// Searches crates.io for `name` and returns at most the configured number
    /// of results, in relevance order. A failed search is retried on the next
    /// call.
    pub async fn search(&self, name: &str) -> Result<Arc<[Crate]>, String> {
        let limit = self.per_page.load(Ordering::Relaxed);
        let query = name.to_owned();
        self.searches
            .get_or_retry(name.to_owned(), move || {
                api::search(&GlobalContext::default()?, CRATES_IO_API, &query, limit).map(Arc::from)
            })
            .await
    }

    /// Returns the crates.io readme of one version as Markdown, or `None`
    /// when the fetch fails. A failed fetch is retried on the next call.
    pub async fn get_readme(&self, name: &str, version: &str) -> Option<String> {
        let key = (name.to_owned(), version.to_owned());
        let (name, version) = key.clone();
        self.readmes
            .get_or_retry(key, move || {
                api::readme(&GlobalContext::default()?, CRATES_IO_READMES, &name, &version)
            })
            .await
            .ok()
    }

    /// Returns the repository link that crates.io lists for `name`.
    pub async fn get_crate_repository(&self, name: &str) -> Option<String> {
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || {
            api::repository(&GlobalContext::default()?, CRATES_IO_API, &name)
        })
        .await
        .ok()?
        .ok()?
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use anyhow::anyhow;
    use rstest::rstest;

    use super::*;

    #[tokio::test]
    async fn cache_runs_one_fetch_per_key() {
        let cache = Cache::<&str, u32>::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let fetch = |calls: Arc<AtomicUsize>| {
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(7)
            }
        };
        assert!(matches!(cache.peek(&"a").await, CacheItemOut::NotStarted));
        let (first, second) = tokio::join!(
            cache.get("a", fetch(calls.clone())),
            cache.get("a", fetch(calls.clone())),
        );
        assert_eq!((first, second), (Ok(7), Ok(7)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(cache.peek(&"a").await, CacheItemOut::Ready(7)));
    }

    #[derive(Clone, Copy)]
    enum Call {
        Get,
        GetThenClear,
        GetOrRetry,
    }

    #[rstest]
    #[case::get_keeps_error(Call::Get, "Error")]
    #[case::clear_forgets(Call::GetThenClear, "NotStarted")]
    #[case::retry_drops_error(Call::GetOrRetry, "NotStarted")]
    #[tokio::test]
    async fn cache_error_state(#[case] call: Call, #[case] expected: &str) {
        let cache = Cache::<&str, u32>::default();
        let fetch = || Err(anyhow!("offline"));
        let result = match call {
            Call::Get | Call::GetThenClear => cache.get("a", fetch).await,
            Call::GetOrRetry => cache.get_or_retry("a", fetch).await,
        };
        assert_eq!(result, Err("offline".to_owned()));
        if let Call::GetThenClear = call {
            cache.clear().await;
        }
        let state = match cache.peek(&"a").await {
            CacheItemOut::NotStarted => "NotStarted",
            CacheItemOut::Pending => "Pending",
            CacheItemOut::Error(_) => "Error",
            CacheItemOut::Ready(_) => "Ready",
        };
        assert_eq!(state, expected);
    }
}
