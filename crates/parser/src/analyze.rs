use std::{
    collections::HashMap,
    sync::{
        Arc,
        Weak,
    },
};

use cargo_util_schemas::core::PartialVersion;
use info_provider::{
    CacheItemOut,
    InfoProvider,
    suggestable,
};
use tokio::sync::RwLock;
use tower_lsp::lsp_types::{
    Diagnostic,
    DiagnosticSeverity,
    MessageType,
    Position,
    Range,
};

use crate::{
    Db,
    Uri,
    structs::version::is_newer,
    toml::DepSource,
    tree::ByteSpan,
};

const DOCS_INFO_MESSAGE: &str = "\nThe first line of every Cargo.toml has code actions that will \
                                 open docs/issues for the cargotom lsp.\n\n To hide this message \
                                 please set hide_docs_info_message. \n\nFor further information \
                                 please check out the docs";

fn crate_not_found_message(
    registry: Option<&str>,
    name: &str,
) -> String
{
    match registry
    {
        Some(registry) => format!("crate {name} not found in registry {registry}"),
        None => format!("crate {name} not found on crates.io"),
    }
}

/// A crate to fetch: its registry, where `None` is
/// crates.io, and its name.
type Missing = (Option<String>, String);

impl Db
{
    /// Checks the dependencies and features of `uri`, or of
    /// every open document when `uri` is `None`, and
    /// publishes the diagnostics.
    ///
    /// A crate that no lookup has fetched yet starts one
    /// fetch in the background, which analyses the document
    /// again when it ends.
    pub async fn analyze(
        &self,
        uri: Option<Uri>,
    )
    {
        if let Err(e) = self.client.inlay_hint_refresh().await
        {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!("inlay hint refresh failed: {e}"),
                )
                .await;
        }
        match &uri
        {
            Some(uri) => self.analyze_single(uri).await,
            None =>
            {
                for item in self.files.keys()
                {
                    self.analyze_single(item).await;
                }
            }
        }
    }

    async fn analyze_single(
        &self,
        uri: &Uri,
    )
    {
        let Some(toml) = self.tomls.get(uri)
        else
        {
            return;
        };
        let mut errors: Vec<(ByteSpan, String)> = vec![];
        let mut warnings: Vec<(ByteSpan, String)> = vec![];
        let mut missing: Vec<Missing> = vec![];
        let mut names: HashMap<String, Vec<_>> = HashMap::new();
        let workspace = self.workspaces.get(uri).and_then(|v| self.tomls.get(v));

        for dep in &toml.dependencies
        {
            let mut features: HashMap<&str, Vec<_>> = HashMap::new();
            for feature in &dep.data.features.data
            {
                features.entry(&feature.data).or_default().push(feature);
            }
            for feature in features.into_values().filter(|v| v.len() > 1).flatten()
            {
                errors.push((feature.into(), "Duplicate feature name".to_owned()));
            }
            let targets: Vec<_> = dep.data.target.iter().map(|v| v.data.to_string()).collect();
            names
                .entry(format!(
                    "{}{:?}{}",
                    dep.data.name(),
                    dep.data.kind,
                    targets.join(" ")
                ))
                .or_default()
                .push(dep);
        }
        for dep in names.into_values().filter(|v| v.len() > 1).flatten()
        {
            errors.push((
                dep.data.crate_name_range(),
                "Duplicate dependency name".to_owned(),
            ));
        }

        for dep in &toml.dependencies
        {
            let name = dep.data.crate_name();
            let (source, range, from_workspace) = match &dep.data.source
            {
                DepSource::Workspace(range) =>
                {
                    let Some(workspace) = workspace
                    else
                    {
                        errors.push((*range, "is not part of a workspace".to_owned()));
                        continue;
                    };
                    let Some(root) = workspace
                        .dependencies
                        .iter()
                        .find(|v| v.data.name() == name)
                    else
                    {
                        errors.push((*range, "cannot find the crate in the workspace".to_owned()));
                        continue;
                    };
                    (&root.data.source, Some(*range), true)
                }
                source => (source, source.range(), false),
            };
            let (DepSource::Version { value, registry }, Some(range)) = (source, range)
            else
            {
                continue;
            };
            let registry = registry.as_ref().map(|v| v.data.as_str());
            let items = match self.info.get_info_cache(registry, &name).await
            {
                CacheItemOut::Ready(items) => items,
                CacheItemOut::Pending => continue,
                CacheItemOut::NotStarted =>
                {
                    // The workspace root analysis fetches
                    // its own crates.
                    if !from_workspace
                    {
                        missing.push((registry.map(ToOwned::to_owned), name));
                    }
                    continue;
                }
                CacheItemOut::Error(e) =>
                {
                    let message = snafu::Report::from_error(e.as_ref()).to_string();
                    errors.push((dep.data.crate_name_range(), message));
                    continue;
                }
            };
            if items.is_empty()
            {
                warnings.push((
                    dep.data.crate_name_range(),
                    crate_not_found_message(registry, &name),
                ));
                continue;
            }
            let Ok(requirement) = value.value.data.parse::<PartialVersion>()
            else
            {
                continue;
            };
            let Some(package) = items.iter().rfind(|v| requirement.matches(v.version()))
            else
            {
                errors.push((range, "Invalid version".to_owned()));
                continue;
            };
            for feature in &dep.data.features.data
            {
                if !package.features().contains_key(feature.data.as_str())
                {
                    errors.push((feature.into(), "Unknown Feature".to_owned()));
                }
            }
            let newer = suggestable(&items, self.config.stable_version)
                .rfind(|v| is_newer(&requirement, v.version()));
            if let Some(newer) = newer.filter(|_| self.config.outdated_crate_warnings)
            {
                warnings.push((
                    range,
                    format!("Newer version available: {}", newer.version()),
                ));
            }
        }

        if !missing.is_empty()
        {
            refetch(self.this.clone(), self.info.clone(), uri.clone(), missing);
        }

        let info = (!self.config.hide_docs_info_message).then(|| {
            Diagnostic {
                range: Range::new(Position::new(0, 0), Position::new(1, 0)),
                severity: Some(DiagnosticSeverity::INFORMATION),
                message: DOCS_INFO_MESSAGE.to_owned(),
                ..Default::default()
            }
        });
        let found = warnings
            .into_iter()
            .map(|v| (DiagnosticSeverity::WARNING, v))
            .chain(errors.into_iter().map(|v| (DiagnosticSeverity::ERROR, v)))
            .filter_map(|(severity, (span, message))| {
                Some(Diagnostic {
                    range: self.get_range(uri, span)?,
                    severity: Some(severity),
                    message,
                    ..Default::default()
                })
            });
        let diagnostics = info.into_iter().chain(found).collect();
        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;
    }
}

/// Fetches every crate in `missing` on one background task,
/// then analyses `uri` again. A failed fetch is logged, and
/// the cache keeps the error for the next analysis to show.
fn refetch(
    db: Weak<RwLock<Db>>,
    info: Arc<InfoProvider>,
    uri: Uri,
    missing: Vec<Missing>,
)
{
    tokio::spawn(async move {
        let mut failures = vec![];
        for (registry, name) in &missing
        {
            if let Err(e) = info.get_info(registry.as_deref(), name).await
            {
                failures.push(snafu::Report::from_error(e.as_ref()).to_string());
            }
        }
        let Some(db) = db.upgrade()
        else
        {
            return;
        };
        let db = db.read().await;
        for failure in failures
        {
            db.client.log_message(MessageType::ERROR, failure).await;
        }
        db.analyze_single(&uri).await;
    });
}
