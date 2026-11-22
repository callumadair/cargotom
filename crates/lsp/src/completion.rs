//! Completions for the cursor position in a manifest.

use std::{
    collections::HashSet,
    path::PathBuf,
};

use cargo_util_schemas::core::PartialVersion;
use info_provider::Crate;
use parser::{
    Db,
    static_structure,
    toml::{
        DepSource,
        Dependency,
        ManifestTextError,
        OptionalKey,
        Positioned,
        Toml,
    },
};
use snafu::ResultExt;
use tower_lsp::lsp_types::{
    CompletionItem,
    CompletionItemKind,
    CompletionList,
    CompletionResponse,
    CompletionTextEdit,
    MessageType,
    Position,
    Range,
    TextEdit,
    Url,
};

use crate::{
    actions::manifest_dir,
    lsp::Context,
    utils::{
        ContextSnafu,
        EditError,
        report,
    },
};

/// What the cursor completes. The handler gathers it under
/// the db lock and runs any registry lookup without it.
enum Target
{
    /// Completions that need no lookup.
    Ready(Vec<CompletionItem>),
    /// A dependency name completes to a crate that the
    /// search finds.
    Name
    {
        dep:          Dependency,
        range:        Range,
        typed:        String,
        exclude:      HashSet<String>,
        in_workspace: bool,
        dir:          PathBuf,
    },
    /// A version requirement completes to a published
    /// version.
    Version
    {
        dep:   Dependency,
        range: Range,
        typed: String,
        dir:   PathBuf,
    },
    /// A dependency feature completes to a feature of the
    /// matching version.
    Feature
    {
        registry:    Option<String>,
        name:        String,
        requirement: Option<PartialVersion>,
        range:       Range,
        typed:       String,
        listed:      Vec<String>,
    },
}

/// Strips the build metadata from a version, which a
/// requirement cannot hold.
fn dependency_requirement(version: &str) -> &str
{
    version
        .split_once('+')
        .map_or(version, |(version, _)| version)
}

/// Returns the part of `text` before the cursor at
/// `offset`, where `text` starts at `start`.
fn typed(
    text: &str,
    start: u32,
    offset: usize,
) -> String
{
    let end = offset.saturating_sub(start as usize);
    text.get(..end).unwrap_or(text).to_owned()
}

/// Returns an item that replaces `range` with `new_text`.
fn edit_item(
    label: String,
    range: Range,
    new_text: String,
) -> CompletionItem
{
    CompletionItem {
        label,
        kind: Some(CompletionItemKind::MODULE),
        text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(range, new_text))),
        ..CompletionItem::default()
    }
}

/// Returns an incomplete list, so that the client asks
/// again as the user types.
fn list(items: Vec<CompletionItem>) -> CompletionResponse
{
    CompletionResponse::List(CompletionList {
        is_incomplete: true,
        items,
    })
}

impl Context
{
    /// Returns the completions at `position` of `uri`.
    pub async fn complete(
        &self,
        uri: &Url,
        position: Position,
    ) -> Option<CompletionResponse>
    {
        let target = {
            let db = self.db.read().await;
            target(&db, uri, position)?
        };
        let items = match target
        {
            Target::Ready(items) => Ok(items),
            Target::Name {
                dep,
                range,
                typed,
                exclude,
                in_workspace,
                dir,
            } =>
            {
                let found = self.info.search(&typed).await;
                let found = self.log_failure(found).await?;
                self.manifest_items(|gctx| {
                    name_items(gctx, &found, &dep, range, &exclude, in_workspace, &dir)
                })
            }
            Target::Version {
                dep,
                range,
                typed,
                dir,
            } =>
            {
                let found = self
                    .info
                    .get_info(dep.source.registry(), &dep.crate_name())
                    .await;
                let found = self.log_failure(found).await?;
                self.manifest_items(|gctx| {
                    found
                        .iter()
                        .rev()
                        .map(|v| v.version().to_string())
                        .filter(|v| v.starts_with(&typed))
                        .enumerate()
                        .map(|(i, v)| {
                            let mut dep = dep.clone();
                            dep.source.set_version(OptionalKey::no_key(Positioned::new(
                                0,
                                0,
                                v.clone(),
                            )));
                            let text = dep.to_manifest_text(gctx, &dir, &dir)?;
                            Ok(CompletionItem {
                                sort_text: Some(format!("{i:06}")),
                                ..edit_item(v, range, text)
                            })
                        })
                        .collect()
                })
            }
            Target::Feature {
                registry,
                name,
                requirement,
                range,
                typed,
                listed,
            } =>
            {
                let found = self.info.get_info(registry.as_deref(), &name).await;
                let found = self.log_failure(found).await?;
                let features = found
                    .iter()
                    .rfind(|v| {
                        requirement
                            .as_ref()
                            .is_some_and(|req| req.matches(v.version()))
                    })
                    .map(|v| {
                        v.features()
                            .keys()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                Ok(features
                    .into_iter()
                    .filter(|v| v.starts_with(&typed) && !listed.contains(v))
                    .map(|v| {
                        let text = format!("\"{v}\"");
                        edit_item(v, range, text)
                    })
                    .collect())
            }
        };
        match items
        {
            Ok(items) => Some(list(items)),
            Err(e) =>
            {
                self.client
                    .log_message(MessageType::ERROR, report(&e))
                    .await;
                None
            }
        }
    }

    /// Builds items with a fresh cargo config. The config
    /// is not `Send`, so it lives only inside this
    /// synchronous call.
    fn manifest_items(
        &self,
        build: impl FnOnce(&cargo::GlobalContext) -> Result<Vec<CompletionItem>, ManifestTextError>,
    ) -> Result<Vec<CompletionItem>, EditError>
    {
        let gctx = self.info.context().context(ContextSnafu)?;
        Ok(build(&gctx)?)
    }

    /// Logs a failed lookup and returns its value.
    async fn log_failure<T, E: std::error::Error + 'static>(
        &self,
        result: Result<T, std::sync::Arc<E>>,
    ) -> Option<T>
    {
        match result
        {
            Ok(v) => Some(v),
            Err(e) =>
            {
                self.client
                    .log_message(MessageType::WARNING, report(e.as_ref()))
                    .await;
                None
            }
        }
    }
}

/// Returns an item for each crate in `found` that the
/// manifest does not list.
fn name_items(
    gctx: &cargo::GlobalContext,
    found: &[Crate],
    dep: &Dependency,
    range: Range,
    exclude: &HashSet<String>,
    in_workspace: bool,
    dir: &std::path::Path,
) -> Result<Vec<CompletionItem>, ManifestTextError>
{
    found
        .iter()
        .filter(|v| !exclude.contains(&v.name))
        .enumerate()
        .map(|(i, v)| {
            let mut dep = dep.clone();
            dep.set_name(v.name.clone());
            if let DepSource::None = dep.source
            {
                dep.source = if in_workspace
                {
                    DepSource::Workspace(Default::default())
                }
                else
                {
                    let version = dependency_requirement(&v.max_version).to_owned();
                    DepSource::Version {
                        value:    OptionalKey::no_key(Positioned::new(0, 0, version)),
                        registry: None,
                    }
                };
            }
            let text = dep.to_manifest_text(gctx, dir, dir)?;
            Ok(CompletionItem {
                detail: v.description.clone(),
                sort_text: Some(format!("{i:06}")),
                kind: None,
                ..edit_item(format!("{i}. {}", v.name), range, text)
            })
        })
        .collect()
}

/// Returns what the cursor at `position` completes, or
/// `None` when nothing completes there.
fn target(
    db: &Db,
    uri: &Url,
    position: Position,
) -> Option<Target>
{
    let toml = db.get_toml(uri)?;
    let workspace = db.get_workspace(uri).and_then(|v| db.get_toml(v));
    let offset = db.get_byte(uri, position)?;
    if let Some(dep) = toml.dependencies.iter().find(|v| v.touches(offset))
    {
        return dependency_target(db, uri, toml, workspace, dep, offset);
    }
    if let Some(items) = feature_arg_items(db, uri, toml, offset)
    {
        return Some(Target::Ready(items));
    }
    let mut path = db.get_path(uri, position)?;
    let is_value = path.pop().is_some_and(|v| v.is_value(path.last()));
    let keys: Vec<String> = path.iter().map(|v| v.ty.to_string()).collect();
    // `initialize` reports a schema that does not parse.
    let items = static_structure::schema()
        .ok()?
        .completions(&keys, 0, is_value)
        .unwrap_or_default()
        .into_iter()
        .map(|label| {
            CompletionItem {
                label,
                ..CompletionItem::default()
            }
        })
        .collect();
    Some(Target::Ready(items))
}

/// Returns what the cursor at `offset` inside `dep`
/// completes.
fn dependency_target(
    db: &Db,
    uri: &Url,
    toml: &Toml,
    workspace: Option<&Toml>,
    dep: &Positioned<Dependency>,
    offset: usize,
) -> Option<Target>
{
    let data = &dep.data;
    let name_span = data.crate_name_range();
    if name_span.touches(offset)
    {
        let exclude = toml
            .dependencies
            .iter()
            .filter(|v| v.data.crate_name_range() != name_span)
            .map(|v| v.data.name().to_owned())
            .collect();
        return Some(Target::Name {
            dep: data.clone(),
            range: db.get_range(uri, dep.into())?,
            typed: typed(data.name(), name_span.start, offset),
            exclude,
            in_workspace: workspace.is_some(),
            dir: manifest_dir(uri)?,
        });
    }
    if let DepSource::Version { value, .. } = &data.source
        && value.value.touches(offset)
    {
        // The value starts at its opening quote.
        return Some(Target::Version {
            dep:   data.clone(),
            range: db.get_range(uri, dep.into())?,
            typed: typed(&value.value.data, value.value.start + 1, offset),
            dir:   manifest_dir(uri)?,
        });
    }
    let feature = data.features.data.iter().find(|v| v.touches(offset))?;
    let source = match &data.source
    {
        DepSource::Workspace(_) =>
        {
            workspace.and_then(|v| {
                v.dependencies
                    .iter()
                    .find(|v| v.data.name() == data.crate_name())
                    .map(|v| &v.data.source)
            })
        }
        source => Some(source),
    };
    let Some(DepSource::Version { value, registry }) = source
    else
    {
        return None;
    };
    Some(Target::Feature {
        registry:    registry.as_ref().map(|v| v.data.clone()),
        name:        data.crate_name(),
        requirement: value.value.data.parse().ok(),
        range:       db.get_range(uri, feature.into())?,
        typed:       typed(&feature.data, feature.start + 1, offset),
        listed:      data.features.data.iter().map(|v| v.data.clone()).collect(),
    })
}

/// Returns the completions for a value in a `[features]`
/// array: another feature of this manifest, or `dep:` and
/// an optional dependency.
fn feature_arg_items(
    db: &Db,
    uri: &Url,
    toml: &Toml,
    offset: usize,
) -> Option<Vec<CompletionItem>>
{
    let (feat, arg) = toml
        .features
        .iter()
        .flat_map(|feat| feat.data.args.iter().map(move |arg| (feat, arg)))
        .find(|(_, arg)| arg.touches(offset))?;
    let text = arg.data.to_string();
    let typed = typed(&text, arg.start + 1, offset);
    let listed: Vec<String> = feat.data.args.iter().map(|v| v.data.to_string()).collect();
    let features = toml
        .features
        .iter()
        .map(|v| v.data.name.data.clone())
        .filter(|name| *name != feat.data.name.data);
    let optional = toml
        .dependencies
        .iter()
        .filter(|dep| dep.data.optional.as_ref().is_some_and(|v| v.data))
        .map(|dep| format!("dep:{}", dep.data.name()));
    let range = db.get_range(uri, arg.into())?;
    Some(
        features
            .chain(optional)
            .filter(|v| v.starts_with(&typed) && !listed.contains(v))
            .map(|v| {
                let text = format!("\"{v}\"");
                edit_item(v, range, text)
            })
            .collect(),
    )
}
