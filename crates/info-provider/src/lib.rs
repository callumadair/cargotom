//! Crate metadata for the language server.
//!
//! Versions and features come from the registry index
//! through cargo. Search results, readmes and repository
//! links come from crates.io. Every lookup
//! runs on a blocking thread and reads the cargo config of
//! the user.

use std::{
    collections::HashMap,
    hash::Hash,
    sync::{
        Arc,
        atomic::{
            AtomicU32,
            Ordering,
        },
    },
};

pub use cargo::workspace::Summary;
use cargo::{
    GlobalContext,
    util::{
        CargoResult,
        network::http_async,
    },
};
pub use crates_io::Crate;
use snafu::{
    ResultExt,
    Snafu,
};
use tokio::sync::{
    Mutex,
    OnceCell,
};

pub mod api;

const CRATES_IO_API: &str = "https://crates.io";
const CRATES_IO_READMES: &str = "https://static.crates.io/readmes";

/// An error from cargo, which reports every failure as one
/// opaque type.
pub type Cause = Box<dyn std::error::Error + Send + Sync>;

/// Returns the versions that a suggestion can name, in the
/// order of `summaries`. With `stable_only` a pre-release
/// is left out.
pub fn suggestable(
    summaries: &[Summary],
    stable_only: bool,
) -> impl DoubleEndedIterator<Item = &Summary>
{
    summaries
        .iter()
        .filter(move |v| !stable_only || v.version().pre.is_empty())
}

/// A failed crate data lookup.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum InfoError
{
    /// Cargo could not load its config or start its HTTP
    /// client.
    #[snafu(display("cannot set up cargo"))]
    Cargo
    {
        source: Cause
    },
    /// The registry index query for a crate failed.
    #[snafu(display("cannot query the registry index for `{name}`"))]
    Index
    {
        name: String, source: Cause
    },
    /// The request for `url` could not be built.
    #[snafu(display("cannot build a request for {url}"))]
    Request
    {
        url: String, source: http::Error
    },
    /// The request for `url` failed in transit.
    #[snafu(display("GET {url} failed"))]
    Http
    {
        url:    String,
        source: http_async::Error,
    },
    /// The server answered with a status other than 200.
    #[snafu(display("GET {url} returned {status}"))]
    Status
    {
        url:    String,
        status: http::StatusCode,
    },
    /// The response body was not the expected JSON.
    #[snafu(display("cannot parse the response of {url}"))]
    Json
    {
        url:    String,
        source: serde_json::Error,
    },
    /// The crates.io search failed.
    #[snafu(display("cannot search crates.io for `{query}`"))]
    Search
    {
        query:  String,
        source: crates_io::Error<http_async::Error>,
    },
    /// The blocking task that ran the lookup panicked or
    /// was cancelled.
    #[snafu(display("the lookup task did not finish"))]
    Join
    {
        source: tokio::task::JoinError
    },
}

/// The state of one cached lookup.
pub enum CacheItemOut<V>
{
    /// No fetch has started for the key.
    NotStarted,
    /// A fetch is running.
    Pending,
    /// The last fetch failed. The next [`InfoProvider`]
    /// call for the key fetches again.
    Error(Arc<InfoError>),
    /// The fetch returned this value.
    Ready(V),
}

type Entry<V> = Arc<OnceCell<Result<V, Arc<InfoError>>>>;

/// Runs one blocking fetch per key and keeps the result.
/// Concurrent callers for one key share a single fetch. A
/// failed result stays visible to [`Cache::peek`] until the
/// next [`Cache::get`] replaces it.
#[derive(Default)]
struct Cache<K, V>(Mutex<HashMap<K, Entry<V>>>);

impl<K: Eq + Hash, V: Clone + Send + 'static> Cache<K, V>
{
    async fn peek(
        &self,
        key: &K,
    ) -> CacheItemOut<V>
    {
        let state = self.0.lock().await.get(key).map(|cell| cell.get().cloned());
        match state
        {
            None => CacheItemOut::NotStarted,
            Some(None) => CacheItemOut::Pending,
            Some(Some(Ok(value))) => CacheItemOut::Ready(value),
            Some(Some(Err(e))) => CacheItemOut::Error(e),
        }
    }

    async fn get(
        &self,
        key: K,
        fetch: impl FnOnce() -> Result<V, InfoError> + Send + 'static,
    ) -> Result<V, Arc<InfoError>>
    {
        let cell = {
            let mut map = self.0.lock().await;
            let cell = map.entry(key).or_default();
            if matches!(cell.get(), Some(Err(_)))
            {
                *cell = Entry::default();
            }
            cell.clone()
        };
        cell.get_or_init(|| {
            async move {
                tokio::task::spawn_blocking(fetch)
                    .await
                    .context(JoinSnafu)
                    .flatten()
                    .map_err(Arc::new)
            }
        })
        .await
        .clone()
    }

    async fn clear(&self) { self.0.lock().await.clear(); }
}

/// Builds the cargo context for one lookup.
type ContextFactory = dyn Fn() -> CargoResult<GlobalContext> + Send + Sync;

/// Fetches and caches crate data for the language server.
///
/// Every method is safe to call from many tasks at once. A
/// blocking fetch runs on the tokio blocking pool, so the
/// methods need a tokio runtime.
pub struct InfoProvider
{
    gctx:         Arc<ContextFactory>,
    per_page:     AtomicU32,
    readmes:      Cache<(String, String), String>,
    /// Keyed by registry name, where `None` is crates.io,
    /// and crate name.
    summaries:    Cache<(Option<String>, String), Vec<Summary>>,
    searches:     Cache<String, Arc<[Crate]>>,
    repositories: Cache<String, Option<String>>,
}

impl InfoProvider
{
    /// Returns a provider that reads the cargo config of
    /// the user. `per_page` is the most results that one
    /// search returns.
    pub fn new(per_page: u32) -> Self { Self::with_context(per_page, GlobalContext::default) }

    /// Returns a provider that builds the cargo context of
    /// each lookup with `gctx`. A test can point it at a
    /// temporary cargo home and config.
    pub fn with_context(
        per_page: u32,
        gctx: impl Fn() -> CargoResult<GlobalContext> + Send + Sync + 'static,
    ) -> Self
    {
        Self {
            gctx:         Arc::new(gctx),
            per_page:     AtomicU32::new(per_page),
            readmes:      Cache::default(),
            summaries:    Cache::default(),
            searches:     Cache::default(),
            repositories: Cache::default(),
        }
    }

    /// Builds a cargo context the way this provider does
    /// for its lookups.
    ///
    /// # Errors
    ///
    /// Returns [`InfoError::Cargo`] when cargo cannot load
    /// its config.
    pub fn context(&self) -> Result<GlobalContext, InfoError>
    {
        (self.gctx)().map_err(Cause::from).context(CargoSnafu)
    }

    /// Returns a closure that runs `call` with a fresh
    /// cargo context, for use on a blocking thread.
    fn blocking<T>(
        &self,
        call: impl FnOnce(&GlobalContext) -> Result<T, InfoError> + Send + 'static,
    ) -> impl FnOnce() -> Result<T, InfoError> + Send + 'static
    {
        let gctx = self.gctx.clone();
        move || call(&gctx().map_err(Cause::from).context(CargoSnafu)?)
    }

    /// Sets the search result limit and drops the cached
    /// searches.
    pub async fn set_per_page(
        &self,
        per_page: u32,
    )
    {
        self.per_page.store(per_page, Ordering::Relaxed);
        self.searches.clear().await;
    }

    /// Returns the cached result of [`Self::get_info`]
    /// without starting it.
    pub async fn get_info_cache(
        &self,
        registry: Option<&str>,
        name: &str,
    ) -> CacheItemOut<Vec<Summary>>
    {
        self.summaries
            .peek(&(registry.map(ToOwned::to_owned), name.to_owned()))
            .await
    }

    /// Returns every non-yanked version of `name` in
    /// ascending version order.
    ///
    /// `registry` names a registry from the cargo config of
    /// the user, and `None` means crates.io. A crate that
    /// the registry does not have gives an empty list. A
    /// success is cached for the life of the provider, and
    /// a failure is fetched again on the next call.
    ///
    /// # Errors
    ///
    /// Returns the [`InfoError`] of the registry query.
    pub async fn get_info(
        &self,
        registry: Option<&str>,
        name: &str,
    ) -> Result<Vec<Summary>, Arc<InfoError>>
    {
        let key = (registry.map(ToOwned::to_owned), name.to_owned());
        let (registry, name) = key.clone();
        let fetch = self.blocking(move |gctx| api::query_index(gctx, registry.as_deref(), &name));
        self.summaries.get(key, fetch).await
    }

    /// Searches crates.io for `name` and returns at most
    /// the configured number of results, in relevance
    /// order. A failure is fetched again on the next call.
    ///
    /// # Errors
    ///
    /// Returns the [`InfoError`] of the search.
    pub async fn search(
        &self,
        name: &str,
    ) -> Result<Arc<[Crate]>, Arc<InfoError>>
    {
        let limit = self.per_page.load(Ordering::Relaxed);
        let query = name.to_owned();
        let fetch = self
            .blocking(move |gctx| api::search(gctx, CRATES_IO_API, &query, limit).map(Arc::from));
        self.searches.get(name.to_owned(), fetch).await
    }

    /// Returns the crates.io readme of one version as
    /// Markdown. A failure is fetched again on the next
    /// call.
    ///
    /// # Errors
    ///
    /// Returns the [`InfoError`] of the fetch, for example
    /// a 404 when the version has no readme.
    pub async fn get_readme(
        &self,
        name: &str,
        version: &str,
    ) -> Result<String, Arc<InfoError>>
    {
        let key = (name.to_owned(), version.to_owned());
        let (name, version) = key.clone();
        let fetch =
            self.blocking(move |gctx| api::readme(gctx, CRATES_IO_READMES, &name, &version));
        self.readmes.get(key, fetch).await
    }

    /// Returns the repository link that crates.io lists for
    /// `name`, or `None` when the crate lists none. A
    /// failure is fetched again on the next call.
    ///
    /// # Errors
    ///
    /// Returns the [`InfoError`] of the crates.io request.
    pub async fn get_crate_repository(
        &self,
        name: &str,
    ) -> Result<Option<String>, Arc<InfoError>>
    {
        let query = name.to_owned();
        let fetch = self.blocking(move |gctx| api::repository(gctx, CRATES_IO_API, &query));
        self.repositories.get(name.to_owned(), fetch).await
    }
}

#[cfg(test)]
mod tests
{
    use std::sync::atomic::AtomicUsize;

    use super::*;

    fn offline() -> InfoError
    {
        InfoError::Status {
            url:    "https://example.invalid".to_owned(),
            status: http::StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    #[tokio::test]
    async fn cache_runs_one_fetch_per_key()
    {
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
        assert_eq!((first.ok(), second.ok()), (Some(7), Some(7)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(cache.peek(&"a").await, CacheItemOut::Ready(7)));
    }

    #[tokio::test]
    async fn cache_keeps_error_until_next_get()
    {
        let cache = Cache::<&str, u32>::default();
        assert!(cache.get("a", || Err(offline())).await.is_err());
        assert!(matches!(
            cache.peek(&"a").await,
            CacheItemOut::Error(e) if matches!(*e, InfoError::Status { .. })
        ));
        assert_eq!(cache.get("a", || Ok(7)).await.ok(), Some(7));
        assert!(matches!(cache.peek(&"a").await, CacheItemOut::Ready(7)));
    }

    #[tokio::test]
    async fn cache_clear_forgets()
    {
        let cache = Cache::<&str, u32>::default();
        assert_eq!(cache.get("a", || Ok(7)).await.ok(), Some(7));
        cache.clear().await;
        assert!(matches!(cache.peek(&"a").await, CacheItemOut::NotStarted));
    }
}
