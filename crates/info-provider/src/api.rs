//! Blocking lookups against a registry. Each function takes
//! the cargo context to use, so it reads the cargo config,
//! auth and proxy settings of the user, and each one must
//! run on a blocking thread.

use std::collections::BTreeSet;

use cargo::{
    GlobalContext,
    sources::{
        IndexSummary,
        SourceConfigMap,
        source::QueryKind,
    },
    util::{
        cache_lock::CacheLockMode,
        interning::InternedString,
        network::http_async,
    },
    workspace::{
        Dependency,
        FeatureValue,
        SourceId,
        Summary,
    },
};
use crates_io::{
    Crate,
    Registry,
};
use serde::{
    Deserialize,
    Serialize,
};
use snafu::{
    ResultExt,
    ensure,
};

use crate::{
    CargoSnafu,
    Cause,
    HttpSnafu,
    IndexSnafu,
    InfoError,
    JsonSnafu,
    RequestSnafu,
    SearchSnafu,
    StatusSnafu,
};

/// Sends crates.io API requests through the curl client of
/// cargo.
struct CargoHttp<'gctx>(&'gctx http_async::Client);

impl crates_io::HttpClient for CargoHttp<'_>
{
    type Error = http_async::Error;

    fn request(
        &self,
        req: http::Request<Vec<u8>>,
    ) -> Result<http::Response<Vec<u8>>, Self::Error>
    {
        self.0.request_blocking(req)
    }
}

fn http_client(gctx: &GlobalContext) -> Result<&http_async::Client, InfoError>
{
    gctx.http_async().map_err(Cause::from).context(CargoSnafu)
}

/// Returns the body of a GET to `url`. Any status other
/// than 200 is an error.
fn get(
    gctx: &GlobalContext,
    url: &str,
) -> Result<Vec<u8>, InfoError>
{
    let request = http::Request::get(url)
        .body(Vec::new())
        .context(RequestSnafu { url })?;
    let response = http_client(gctx)?
        .request_blocking(request)
        .context(HttpSnafu { url })?;
    let status = response.status();
    ensure!(status == http::StatusCode::OK, StatusSnafu { url, status });
    Ok(response.into_body())
}

/// Searches the registry API at `host`, for example `https://crates.io`, and
/// returns at most `limit` crates in relevance order.
pub(crate) fn search(
    gctx: &GlobalContext,
    host: &str,
    query: &str,
    limit: u32,
) -> Result<Vec<Crate>, InfoError>
{
    let mut registry =
        Registry::new_handle(host.to_owned(), None, CargoHttp(http_client(gctx)?), false);
    Ok(registry
        .search(query, limit)
        .context(SearchSnafu { query })?
        .0)
}

/// Returns the readme of one version under `base`, for
/// example `https://static.crates.io/readmes`, converted from HTML to Markdown.
pub(crate) fn readme(
    gctx: &GlobalContext,
    base: &str,
    name: &str,
    version: &str,
) -> Result<String, InfoError>
{
    let html = get(gctx, &format!("{base}/{name}/{name}-{version}.html"))?;
    Ok(html2md::parse_html(&String::from_utf8_lossy(&html)))
}

/// Returns the repository link that the registry API at
/// `host` lists for `name`, or `None` when the crate has
/// none.
pub(crate) fn repository(
    gctx: &GlobalContext,
    host: &str,
    name: &str,
) -> Result<Option<String>, InfoError>
{
    let url = format!("{host}/api/v1/crates/{name}");
    let body: serde_json::Value =
        serde_json::from_slice(&get(gctx, &url)?).context(JsonSnafu { url })?;
    Ok(body["crate"]["repository"].as_str().map(ToOwned::to_owned))
}

/// Returns every non-yanked version of `name` in ascending
/// version order.
///
/// `registry` names a registry from the cargo config in
/// `gctx`, and `None` means crates.io. Source replacement
/// and registry auth apply as they do for `cargo`. A crate
/// that the registry does not have gives an empty list.
///
/// This call blocks while another cargo process holds the
/// package cache lock. It must run on a blocking thread of
/// a tokio runtime, because the cargo source query is async
/// and this function waits for it on that runtime.
pub(crate) fn query_index(
    gctx: &GlobalContext,
    registry: Option<&str>,
    name: &str,
) -> Result<Vec<Summary>, InfoError>
{
    let query = || -> cargo::util::CargoResult<Vec<IndexSummary>> {
        let source_id = match registry
        {
            Some(key) => SourceId::alt_registry(gctx, key)?,
            None => SourceId::crates_io_maybe_sparse_http(gctx)?,
        };
        let dep = Dependency::parse(name, None, source_id)?;
        let _lock = gctx.acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?;
        let mut source = SourceConfigMap::new(gctx)?.load(source_id)?;
        source.set_quiet(true);
        source.invalidate_cache();
        // Cargo drives its own queries with this executor,
        // so the calling thread needs no Tokio runtime.
        futures::executor::block_on(source.query_vec(&dep, QueryKind::Exact))
    };
    let mut summaries = query()
        .map_err(Cause::from)
        .context(IndexSnafu { name })?
        .into_iter()
        .filter_map(|summary| {
            match summary
            {
                IndexSummary::Candidate(summary) => Some(summary),
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    summaries.sort_by(|a, b| a.version().cmp(b.version()));
    Ok(summaries)
}

/// Which features of a published version a hover lists.
#[derive(Eq, PartialEq, Serialize, Deserialize, Clone, Copy)]
pub enum ViewMode
{
    /// Named features and every optional dependency.
    All,
    /// Named features and the optional dependencies that no
    /// feature enables with `dep:`.
    UnusedOpt,
    /// Named features only.
    Features,
}

/// Lists the features of `summary` as hover lines, sorted
/// by name.
///
/// A named feature reads `name: [value, ...]`, or `name`
/// when it enables nothing. An optional dependency reads
/// `name*`. `mode` picks which optional dependencies
/// appear.
pub fn feature_lines(
    summary: &Summary,
    mode: ViewMode,
) -> Vec<String>
{
    // Cargo adds an implicit `name = ["dep:name"]` feature
    // for each optional dependency that no feature names
    // with `dep:`.
    let is_implicit = |name: &InternedString, values: &[FeatureValue]| matches!(values, [FeatureValue::Dep { dep_name }] if dep_name == name);
    let mut lines = summary
        .features()
        .iter()
        .filter(|(name, values)| !is_implicit(name, values))
        .map(|(name, values)| {
            match values.as_slice()
            {
                [] => name.to_string(),
                values =>
                {
                    format!(
                        "{name}: [{}]",
                        values
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            }
        })
        .collect::<BTreeSet<_>>();
    let optional = summary
        .dependencies()
        .iter()
        .filter(|dep| dep.is_optional())
        .map(|dep| dep.name_in_toml())
        .filter(|name| {
            match mode
            {
                ViewMode::All => true,
                ViewMode::UnusedOpt =>
                {
                    summary
                        .features()
                        .get(name)
                        .is_some_and(|values| is_implicit(name, values))
                }
                ViewMode::Features => false,
            }
        });
    lines.extend(optional.map(|name| format!("{name}*")));
    lines.into_iter().collect()
}

#[cfg(test)]
mod tests
{
    use std::{
        collections::BTreeMap,
        fs,
    };

    use cargo::{
        util::IntoUrl,
        workspace::PackageId,
    };
    use cargo_util_terminal::Shell;
    use rstest::rstest;
    use wiremock::{
        Mock,
        MockServer,
        ResponseTemplate,
        matchers::{
            method,
            path,
        },
    };

    use super::*;

    type BoxError = Box<dyn std::error::Error + Send + Sync>;

    fn foo_line(
        vers: &str,
        yanked: bool,
    ) -> String
    {
        let cksum = "0".repeat(64);
        format!(
            r#"{{"name":"foo","vers":"{vers}","deps":[],"cksum":"{cksum}","features":{{}},"yanked":{yanked}}}"#
        )
    }

    #[rstest]
    #[case::sorts_and_drops_yanked("foo", &["0.1.0", "1.0.0"])]
    #[case::unknown_crate("bar", &[])]
    #[tokio::test(flavor = "multi_thread")]
    async fn query_index_lists_unyanked_versions(
        #[case] name: &str,
        #[case] expected: &[&str],
    ) -> Result<(), BoxError>
    {
        // The server answers 404 for any other path, which
        // is how a sparse index reports an unknown crate.
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

        let dir = tempfile::tempdir()?;
        fs::create_dir_all(dir.path().join(".cargo"))?;
        fs::write(
            dir.path().join(".cargo").join("config.toml"),
            format!("[registries.test]\nindex = \"sparse+{}/\"\n", server.uri()),
        )?;

        let root = dir.path().to_path_buf();
        let name = name.to_owned();
        let versions = tokio::task::spawn_blocking(move || {
            let gctx = GlobalContext::new(Shell::new(), root.clone(), root.join("cargo-home"));
            query_index(&gctx, Some("test"), &name).map(|summaries| {
                summaries
                    .iter()
                    .map(|summary| summary.version().to_string())
                    .collect::<Vec<_>>()
            })
        })
        .await??;
        assert_eq!(versions, expected);
        Ok(())
    }

    /// Runs `call` on a blocking thread with a cargo
    /// context whose home is a fresh temporary directory.
    async fn with_gctx<T: Send + 'static>(
        call: impl FnOnce(&GlobalContext) -> T + Send + 'static
    ) -> Result<T, BoxError>
    {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        let value = tokio::task::spawn_blocking(move || {
            call(&GlobalContext::new(
                Shell::new(),
                root.clone(),
                root.join("cargo-home"),
            ))
        })
        .await?;
        Ok(value)
    }

    async fn serve(
        route: &str,
        status: u16,
        body: &str,
    ) -> MockServer
    {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_reads_crates_api() -> Result<(), BoxError>
    {
        let body = r#"{"crates":[{"name":"serde","description":"ser","max_version":"1.0.2"},
            {"name":"serde_json","description":null,"max_version":"1.0.1"}],"meta":{"total":2}}"#;
        let server = serve("/api/v1/crates", 200, body).await;
        let host = server.uri();
        let found = with_gctx(move |gctx| {
            search(gctx, &host, "serde", 2).map(|crates| {
                crates
                    .into_iter()
                    .map(|c| (c.name, c.max_version))
                    .collect::<Vec<_>>()
            })
        })
        .await??;
        assert_eq!(
            found,
            [
                ("serde".into(), "1.0.2".into()),
                ("serde_json".into(), "1.0.1".into())
            ]
        );
        Ok(())
    }

    #[rstest]
    #[case::html_to_markdown(200, "<h1>Title</h1>", Some("Title\n=========="))]
    #[case::missing(404, "", None)]
    #[tokio::test(flavor = "multi_thread")]
    async fn readme_converts_html(
        #[case] status: u16,
        #[case] body: &str,
        #[case] expected: Option<&str>,
    ) -> Result<(), BoxError>
    {
        let server = serve("/foo/foo-1.0.0.html", status, body).await;
        let base = server.uri();
        let readme = with_gctx(move |gctx| readme(gctx, &base, "foo", "1.0.0").ok()).await?;
        assert_eq!(readme.as_deref(), expected);
        Ok(())
    }

    #[rstest]
    #[case::listed(
        r#"{"crate":{"repository":"https://example.invalid/foo"}}"#,
        Some("https://example.invalid/foo")
    )]
    #[case::unlisted(r#"{"crate":{"repository":null}}"#, None)]
    #[tokio::test(flavor = "multi_thread")]
    async fn repository_reads_crate_api(
        #[case] body: &str,
        #[case] expected: Option<&str>,
    ) -> Result<(), BoxError>
    {
        let server = serve("/api/v1/crates/foo", 200, body).await;
        let host = server.uri();
        let repo = with_gctx(move |gctx| repository(gctx, &host, "foo")).await??;
        assert_eq!(repo.as_deref(), expected);
        Ok(())
    }

    // `serde` is optional and named with `dep:`, so it has
    // no implicit feature. `json` is optional and named by
    // no feature, so it has one.
    fn summary() -> Result<Summary, BoxError>
    {
        let url = "https://example.invalid/".into_url()?;
        let source = SourceId::for_alt_registry(&url, "test")?;
        let optional = |name: &str| -> Result<Dependency, BoxError> {
            let mut dep = Dependency::parse(name, None, source)?;
            dep.set_optional(true);
            Ok(dep)
        };
        let features = BTreeMap::from([
            ("default".into(), vec!["std".into()]),
            ("std".into(), vec![]),
            (
                "derive".into(),
                vec!["dep:serde".into(), "serde/derive".into()],
            ),
        ]);
        let summary = Summary::new(
            PackageId::new("foo".into(), "1.0.0".parse()?, source),
            vec![optional("serde")?, optional("json")?],
            &features,
            None::<&str>,
            None,
        )?;
        Ok(summary)
    }

    #[rstest]
    #[case::features(ViewMode::Features, &[])]
    #[case::unused_opt(ViewMode::UnusedOpt, &["json*"])]
    #[case::all(ViewMode::All, &["json*", "serde*"])]
    fn feature_lines_by_mode(
        #[case] mode: ViewMode,
        #[case] optional: &[&str],
    ) -> Result<(), BoxError>
    {
        let mut expected = vec!["default: [std]", "derive: [dep:serde, serde/derive]", "std"];
        expected.extend(optional);
        expected.sort();
        assert_eq!(feature_lines(&summary()?, mode), expected);
        Ok(())
    }
}
