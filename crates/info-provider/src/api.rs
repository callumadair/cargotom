use std::{collections::BTreeSet, sync::Arc};

use cargo::{
    GlobalContext,
    sources::{IndexSummary, SourceConfigMap, source::QueryKind},
    util::{CargoResult, cache_lock::CacheLockMode, interning::InternedString},
    workspace::{Dependency, FeatureValue, SourceId, Summary},
};
use reqwest::{header::USER_AGENT, Client};
use serde::{Deserialize, Serialize};
use tokio::{runtime::Handle, sync::Notify};

use crate::InfoProvider;

pub enum CacheItem<T> {
    Pending(Arc<Notify>),
    Error(String),
    Ready(Vec<T>),
}

pub enum CacheItemOut<T> {
    NotStarted,
    Pending,
    Error(String),
    Ready(Vec<T>),
}

impl InfoProvider {
    pub async fn get_readme_api(&self, crate_name: &str, version: &str) -> Option<String> {
        let key = (crate_name.to_owned(), version.to_owned());
        let fetch = {
            let lock = self.readme_cache.read().await;
            match lock.get(&key) {
                Some(v) => match v {
                    CacheItem::Pending(n) => Some(n.clone()),
                    CacheItem::Ready(items) => return items.first().cloned(),
                    CacheItem::Error(_) => unreachable!(),
                },
                None => None,
            }
        };
        let n = match fetch {
            Some(n) => n,
            None => {
                let n = Arc::new(Notify::new());
                let notify = n.clone();
                {
                    let mut lock = self.readme_cache.write().await;
                    lock.insert(key.clone(), CacheItem::Pending(notify.clone()));
                }
                let client = self.client.clone();
                let key = key.clone();
                let readme_cache = self.readme_cache.clone();
                tokio::spawn(async move {
                    let info = readme(&client, &key.0, &key.1).await;
                    let mut lock = readme_cache.write().await;
                    match &info {
                        Ok(items) => {
                            lock.insert(key, CacheItem::Ready(vec![items.clone()]));
                        }
                        Err(_) => {
                            lock.remove(&key);
                        }
                    }
                    notify.notify_waiters();
                });

                n
            }
        };
        n.notified().await;
        let lock = self.readme_cache.read().await;
        match lock.get(&key) {
            Some(v) => match v {
                CacheItem::Pending(_) => None,
                CacheItem::Ready(items) => items.first().cloned(),
                CacheItem::Error(_) => unreachable!(),
            },
            None => None,
        }
    }

    pub async fn get_crate_repository_api(&self, crate_name: &str) -> Option<String> {
        let url = format!("https://crates.io/api/v1/crates/{}", crate_name);

        let response = self
            .client
            .get(&url)
            .header(USER_AGENT, "zed")
            .send()
            .await
            .ok()?;
        let json: serde_json::Value = response.json().await.ok()?;
        json["crate"]["repository"].as_str().map(|s| s.to_string())
    }

    pub async fn get_info_cache_api(
        &self,
        registry: Option<&str>,
        name: &str,
    ) -> CacheItemOut<Summary> {
        let key = (registry.map(ToOwned::to_owned), name.to_owned());
        match self.info_cache.read().await.get(&key) {
            None => CacheItemOut::NotStarted,
            Some(CacheItem::Pending(_)) => CacheItemOut::Pending,
            Some(CacheItem::Ready(items)) => CacheItemOut::Ready(items.clone()),
            Some(CacheItem::Error(err)) => CacheItemOut::Error(err.clone()),
        }
    }

    pub async fn search_api(&self, name: &str) -> Result<Vec<Crate>, anyhow::Error> {
        let fetch = {
            let lock = self.search_cache.read().await;
            match lock.get(name) {
                Some(v) => match v {
                    CacheItem::Pending(n) => Some(n.clone()),
                    CacheItem::Ready(items) => return Ok(items.clone()),
                    CacheItem::Error(_) => unreachable!(),
                },
                None => None,
            }
        };
        let n = match fetch {
            Some(n) => n,
            None => {
                let n = Arc::new(Notify::new());
                let notify = n.clone();
                {
                    let mut lock = self.search_cache.write().await;
                    lock.insert(name.to_owned(), CacheItem::Pending(notify.clone()));
                }
                let client = self.client.clone();
                let per_page = *self.per_page.read().await;
                let name = name.to_owned();
                let search_cache = self.search_cache.clone();
                tokio::spawn(async move {
                    let info = search(&client, per_page, &name).await;
                    let mut lock = search_cache.write().await;
                    match &info {
                        Ok(items) => {
                            lock.insert(name, CacheItem::Ready(items.clone()));
                        }
                        Err(_) => {
                            lock.remove(&name);
                        }
                    }
                    notify.notify_waiters();
                });

                n
            }
        };
        n.notified().await;
        let lock = self.search_cache.read().await;
        match lock.get(name) {
            Some(v) => match v {
                CacheItem::Pending(_) => return Ok(vec![]),
                CacheItem::Ready(items) => return Ok(items.clone()),
                CacheItem::Error(_) => unreachable!(),
            },
            None => return Ok(vec![]),
        }
    }

    pub async fn get_info_api(
        &self,
        registry: Option<&str>,
        name: &str,
    ) -> Result<Vec<Summary>, String> {
        let key = (registry.map(ToOwned::to_owned), name.to_owned());
        let fetch = match self.info_cache.read().await.get(&key) {
            Some(CacheItem::Pending(n)) => Some(n.clone()),
            Some(CacheItem::Ready(items)) => return Ok(items.clone()),
            Some(CacheItem::Error(e)) => return Err(e.clone()),
            None => None,
        };
        let n = match fetch {
            Some(n) => n,
            None => {
                let n = Arc::new(Notify::new());
                let notify = n.clone();
                let info_cache = self.info_cache.clone();
                self.info_cache
                    .write()
                    .await
                    .insert(key.clone(), CacheItem::Pending(notify.clone()));
                let task_key = key.clone();
                tokio::spawn(async move {
                    let query_key = task_key.clone();
                    let info = tokio::task::spawn_blocking(move || {
                        query_index(
                            &GlobalContext::default()?,
                            query_key.0.as_deref(),
                            &query_key.1,
                        )
                    })
                    .await;
                    let item = match info {
                        Ok(Ok(v)) => CacheItem::Ready(v),
                        Ok(Err(e)) => CacheItem::Error(format!("{e:#}")),
                        Err(e) => CacheItem::Error(e.to_string()),
                    };
                    info_cache.write().await.insert(task_key, item);
                    notify.notify_waiters();
                });
                n
            }
        };

        n.notified().await;
        let lock = self.info_cache.read().await;
        match lock.get(&key) {
            Some(v) => match v {
                CacheItem::Pending(_) => return Ok(vec![]),
                CacheItem::Ready(items) => return Ok(items.clone()),
                CacheItem::Error(e) => return Err(e.clone()),
            },
            None => return Ok(vec![]),
        }
    }
}

async fn readme(client: &Client, crate_name: &str, version: &str) -> reqwest::Result<String> {
    let url = format!("https://static.crates.io/readmes/{crate_name}/{crate_name}-{version}.html");
    client
        .get(&url)
        .header(USER_AGENT, "zed")
        .send()
        .await?
        .text()
        .await
        .map(|v| html2md::parse_html(&v))
}

async fn search(client: &Client, per_page: usize, name: &str) -> Result<Vec<Crate>, anyhow::Error> {
    let url = format!(
        "https://crates.io/api/v1/crates?page=1&per_page={}&q={}&sort=relevance",
        per_page,
        urlencoding::encode(name)
    );
    let res = client.get(&url).header(USER_AGENT, "zed").send().await;
    let res: Result<SearchResponse, _> = match res {
        Ok(v) => v.json().await,
        Err(e) => Err(e),
    };
    Ok(res?.crates)
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Crate {
    pub exact_match: bool,
    pub name: String,
    pub description: Option<String>,
    pub max_stable_version: Option<String>,
    pub max_version: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct SearchResponse {
    crates: Vec<Crate>,
}

/// Returns every non-yanked version of `name` in ascending version order.
///
/// `registry` names a registry from the cargo config in `gctx`, and `None`
/// means crates.io. Source replacement and registry auth apply as they do for
/// `cargo`. A crate that the registry does not have gives an empty list.
///
/// This call blocks while another cargo process holds the package cache lock,
/// and it must run on a blocking thread of a tokio runtime.
fn query_index(
    gctx: &GlobalContext,
    registry: Option<&str>,
    name: &str,
) -> CargoResult<Vec<Summary>> {
    let source_id = match registry {
        Some(key) => SourceId::alt_registry(gctx, key)?,
        None => SourceId::crates_io_maybe_sparse_http(gctx)?,
    };
    let dep = Dependency::parse(name, None, source_id)?;
    let _lock = gctx.acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?;
    let mut source = SourceConfigMap::new(gctx)?.load(source_id)?;
    source.set_quiet(true);
    source.invalidate_cache();
    let mut summaries = Handle::current()
        .block_on(source.query_vec(&dep, QueryKind::Exact))?
        .into_iter()
        .filter_map(|summary| match summary {
            IndexSummary::Candidate(summary) => Some(summary),
            _ => None,
        })
        .collect::<Vec<_>>();
    summaries.sort_by(|a, b| a.version().cmp(b.version()));
    Ok(summaries)
}

/// Which features of a published version a hover lists.
#[derive(Eq, PartialEq, Serialize, Deserialize, Clone, Copy)]
pub enum ViewMode {
    /// Named features and every optional dependency.
    All,
    /// Named features and the optional dependencies that no feature enables
    /// with `dep:`.
    UnusedOpt,
    /// Named features only.
    Features,
}

/// Lists the features of `summary` as hover lines, sorted by name.
///
/// A named feature reads `name: [value, ...]`, or `name` when it enables
/// nothing. An optional dependency reads `name*`. `mode` picks which optional
/// dependencies appear.
pub fn feature_lines(summary: &Summary, mode: ViewMode) -> Vec<String> {
    // Cargo adds an implicit `name = ["dep:name"]` feature for each optional
    // dependency that no feature names with `dep:`.
    let is_implicit = |name: &InternedString, values: &[FeatureValue]| {
        matches!(values, [FeatureValue::Dep { dep_name }] if dep_name == name)
    };
    let mut lines = summary
        .features()
        .iter()
        .filter(|(name, values)| !is_implicit(name, values))
        .map(|(name, values)| match values.as_slice() {
            [] => name.to_string(),
            values => format!(
                "{name}: [{}]",
                values
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
        .collect::<BTreeSet<_>>();
    let optional = summary
        .dependencies()
        .iter()
        .filter(|dep| dep.is_optional())
        .map(|dep| dep.name_in_toml())
        .filter(|name| match mode {
            ViewMode::All => true,
            ViewMode::UnusedOpt => summary
                .features()
                .get(name)
                .is_some_and(|values| is_implicit(name, values)),
            ViewMode::Features => false,
        });
    lines.extend(optional.map(|name| format!("{name}*")));
    lines.into_iter().collect()
}


#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use cargo::{util::IntoUrl, workspace::PackageId};
    use cargo_util_terminal::Shell;
    use rstest::rstest;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;

    fn foo_line(vers: &str, yanked: bool) -> String {
        let cksum = "0".repeat(64);
        format!(
            r#"{{"name":"foo","vers":"{vers}","deps":[],"cksum":"{cksum}","features":{{}},"yanked":{yanked}}}"#
        )
    }

    #[rstest]
    #[case::sorts_and_drops_yanked("foo", &["0.1.0", "1.0.0"])]
    #[case::unknown_crate("bar", &[])]
    #[tokio::test(flavor = "multi_thread")]
    async fn query_index_lists_unyanked_versions(#[case] name: &str, #[case] expected: &[&str]) {
        // The server answers 404 for any other path, which is how a sparse
        // index reports an unknown crate.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/config.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"dl":"https://example.invalid/{crate}/{version}"}"#),
            )
            .mount(&server)
            .await;
        let lines = [
            foo_line("1.0.0", false),
            foo_line("0.2.0", true),
            foo_line("0.1.0", false),
        ];
        Mock::given(method("GET"))
            .and(path("/3/f/foo"))
            .respond_with(ResponseTemplate::new(200).set_body_string(lines.join("\n")))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".cargo")).unwrap();
        fs::write(
            dir.path().join(".cargo").join("config.toml"),
            format!("[registries.test]\nindex = \"sparse+{}/\"\n", server.uri()),
        )
        .unwrap();

        let root = dir.path().to_path_buf();
        let name = name.to_owned();
        let versions = tokio::task::spawn_blocking(move || {
            let gctx = GlobalContext::new(Shell::new(), root.clone(), root.join("cargo-home"));
            query_index(&gctx, Some("test"), &name)
                .unwrap()
                .iter()
                .map(|summary| summary.version().to_string())
                .collect::<Vec<_>>()
        })
        .await
        .unwrap();
        assert_eq!(versions, expected);
    }

    // `serde` is optional and named with `dep:`, so it has no implicit
    // feature. `json` is optional and named by no feature, so it has one.
    fn summary() -> Summary {
        let url = "https://example.invalid/".into_url().unwrap();
        let source = SourceId::for_alt_registry(&url, "test").unwrap();
        let optional = |name: &str| {
            let mut dep = Dependency::parse(name, None, source).unwrap();
            dep.set_optional(true);
            dep
        };
        let features = BTreeMap::from([
            ("default".into(), vec!["std".into()]),
            ("std".into(), vec![]),
            ("derive".into(), vec!["dep:serde".into(), "serde/derive".into()]),
        ]);
        Summary::new(
            PackageId::new("foo".into(), "1.0.0".parse().unwrap(), source),
            vec![optional("serde"), optional("json")],
            &features,
            None::<&str>,
            None,
        )
        .unwrap()
    }

    #[rstest]
    #[case::features(ViewMode::Features, &[])]
    #[case::unused_opt(ViewMode::UnusedOpt, &["json*"])]
    #[case::all(ViewMode::All, &["json*", "serde*"])]
    fn feature_lines_by_mode(#[case] mode: ViewMode, #[case] optional: &[&str]) {
        let mut expected = vec!["default: [std]", "derive: [dep:serde, serde/derive]", "std"];
        expected.extend(optional);
        expected.sort();
        assert_eq!(feature_lines(&summary(), mode), expected);
    }
}
