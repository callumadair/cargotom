//! Code actions for the dependency under the cursor.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
};

use cargo::GlobalContext;
use cargo_util_schemas::core::PartialVersion;
use info_provider::suggestable;
use parser::{
    Db,
    structs::version::is_newer,
    toml::{
        DepSource,
        Dependency,
        DependencyKind,
        ManifestTextError,
        Positioned,
    },
    tree::ByteSpan,
};
use tower_lsp::lsp_types::{
    CodeAction,
    CodeActionKind,
    MessageType,
    Position,
    Range,
    TextEdit,
    Url,
    WorkspaceEdit,
};

use crate::{
    lsp::Context,
    utils::{
        command_action,
        report,
    },
};

/// Returns the directory that holds the manifest at `uri`.
pub(crate) fn manifest_dir(uri: &Url) -> Option<PathBuf>
{
    Some(uri.to_file_path().ok()?.parent()?.to_path_buf())
}

/// The code actions for one dependency. The edits and links
/// need only the db, and `lookup` needs a registry lookup
/// after the db lock is released.
#[derive(Default)]
pub(crate) struct DepActions
{
    /// Actions that edit the manifest.
    pub edits:  Vec<CodeAction>,
    /// Actions that open a page of the crate.
    pub links:  Vec<CodeAction>,
    /// The action that a registry lookup completes.
    pub lookup: Option<Lookup>,
}

/// An action that needs the published versions of a crate.
pub(crate) enum Lookup
{
    /// Replaces the requirement at `range` with the newest
    /// version.
    Upgrade
    {
        uri:         Url,
        registry:    Option<String>,
        name:        String,
        requirement: PartialVersion,
        range:       Range,
        stable_only: bool,
    },
    /// Adds the crate to the workspace root at `at`.
    AddToWorkspace
    {
        root:        Url,
        name:        String,
        at:          Position,
        stable_only: bool,
    },
}

/// Returns an action that applies `edits`.
fn edit_action(
    title: impl Into<String>,
    edits: HashMap<Url, Vec<TextEdit>>,
) -> CodeAction
{
    CodeAction {
        title: title.into(),
        kind: Some(CodeActionKind::EMPTY),
        edit: Some(WorkspaceEdit {
            changes: Some(edits),
            ..WorkspaceEdit::default()
        }),
        ..CodeAction::default()
    }
}

/// Returns an action that replaces `range` of `uri` with
/// `new_text`.
fn replace_action(
    title: impl Into<String>,
    uri: &Url,
    range: Range,
    new_text: String,
) -> CodeAction
{
    edit_action(
        title,
        HashMap::from([(uri.clone(), vec![TextEdit::new(range, new_text)])]),
    )
}

/// Returns the actions for the dependency that overlaps
/// `range`, or `None` when no dependency does.
///
/// # Errors
///
/// Returns [`ManifestTextError`] when cargo cannot write an
/// edited dependency.
pub(crate) fn dep_actions(
    db: &Db,
    gctx: &GlobalContext,
    uri: &Url,
    range: Range,
) -> Result<Option<DepActions>, ManifestTextError>
{
    let Some(dep) = db.get_dependency(uri, range)
    else
    {
        return Ok(None);
    };
    let (Some(dep_range), Some(dir)) = (db.get_range(uri, dep.into()), manifest_dir(uri))
    else
    {
        return Ok(None);
    };
    let mut actions = DepActions::default();
    let name = dep.data.crate_name();
    let stable_only = db.config.stable_version;
    match &dep.data.source
    {
        DepSource::Version { value, registry } =>
        {
            let version = &value.value;
            actions.lookup = version
                .data
                .parse()
                .ok()
                .zip(db.get_range(uri, version.into()))
                .map(|(requirement, range)| {
                    Lookup::Upgrade {
                        uri: uri.clone(),
                        registry: registry.as_ref().map(|v| v.data.clone()),
                        name: name.clone(),
                        requirement,
                        range,
                        stable_only,
                    }
                });
            let version = &version.data;
            actions.links = vec![
                command_action(
                    "Open Documentation",
                    "open_url",
                    vec![format!("https://docs.rs/{name}/{version}/").into()],
                ),
                command_action(
                    "Open Source",
                    "open-src",
                    vec![name.clone().into(), version.clone().into()],
                ),
                command_action(
                    "Open crates.io",
                    "open_url",
                    vec![format!("https://crates.io/crates/{name}").into()],
                ),
            ];
        }
        DepSource::Workspace(_) =>
        {
            let root = db.get_workspace(uri);
            let root_toml = root.and_then(|v| db.get_toml(v));
            if let (Some(root), Some(root_toml)) = (root, root_toml)
                && !root_toml.dependencies.iter().any(|v| v.data.name() == name)
                && let Some(at) = root_toml
                    .dependencies
                    .last()
                    .and_then(|last| db.get_offset(root, last.end as usize))
            {
                actions.lookup = Some(Lookup::AddToWorkspace {
                    root: root.clone(),
                    name: name.clone(),
                    at,
                    stable_only,
                });
            }
        }
        _ =>
        {}
    }
    if let Some(action) = make_workspace_dependency(db, gctx, uri, dep, dep_range)?
    {
        actions.edits.push(action);
    }

    let mut expanded = dep.data.clone();
    expanded.expanded = !expanded.expanded;
    expanded.optional = None;
    let title = if dep.data.expanded
    {
        "Collapse"
    }
    else
    {
        "Expand"
    };
    let text = expanded.to_manifest_text(gctx, &dir, &dir)?;
    actions
        .edits
        .push(replace_action(title, uri, dep_range, text));

    let mut optional = dep.data.clone();
    let is_optional = optional.optional.is_some_and(|v| v.data);
    optional.optional = Some(Positioned::new(0, 0, !is_optional));
    let title = if is_optional
    {
        "Remove optional"
    }
    else
    {
        "Make optional"
    };
    let text = optional.to_manifest_text(gctx, &dir, &dir)?;
    actions
        .edits
        .push(replace_action(title, uri, dep_range, text));
    Ok(Some(actions))
}

/// Returns the action that moves `dep` to the workspace
/// root and points the member at it, or `None` when `dep`
/// already uses the workspace or the member has no root.
fn make_workspace_dependency(
    db: &Db,
    gctx: &GlobalContext,
    uri: &Url,
    dep: &Positioned<Dependency>,
    range: Range,
) -> Result<Option<CodeAction>, ManifestTextError>
{
    if let DepSource::Workspace(_) = dep.data.source
    {
        return Ok(None);
    }
    let Some(root) = db.get_workspace(uri)
    else
    {
        return Ok(None);
    };
    let (Some(root_toml), Some(member_dir), Some(root_dir)) =
        (db.get_toml(root), manifest_dir(uri), manifest_dir(root))
    else
    {
        return Ok(None);
    };
    let name = dep.data.crate_name();
    let mut edits: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    if !root_toml.dependencies.iter().any(|v| v.data.name() == name)
    {
        let Some(at) = root_toml
            .dependencies
            .last()
            .and_then(|last| db.get_offset(root, last.end as usize))
        else
        {
            return Ok(None);
        };
        let root_dep = Dependency {
            name:               dep.data.name.clone(),
            package:            dep.data.package.clone(),
            kind:               DependencyKind::Normal,
            source:             dep.data.source.clone(),
            features:           Positioned::new(0, 0, Vec::new()),
            features_key_range: None,
            default_features:   None,
            typing_keys:        Vec::new(),
            optional:           None,
            expanded:           true,
            target:             Arc::default(),
        };
        let text = root_dep.to_manifest_text(gctx, &member_dir, &root_dir)?;
        edits.insert(
            root.clone(),
            vec![TextEdit::new(Range::new(at, at), format!("\n{text}"))],
        );
    }
    let mut member = dep.data.clone();
    member.source = DepSource::Workspace(ByteSpan::default());
    let text = member.to_manifest_text(gctx, &member_dir, &member_dir)?;
    edits
        .entry(uri.clone())
        .or_default()
        .push(TextEdit::new(range, text));
    Ok(Some(edit_action("Make to workspace dependency", edits)))
}

impl Context
{
    /// Fetches the versions that `lookup` needs and returns
    /// its action, or `None` when no version qualifies. A
    /// failed fetch is logged.
    pub(crate) async fn lookup_action(
        &self,
        lookup: Lookup,
    ) -> Option<CodeAction>
    {
        let (registry, name, stable_only) = match &lookup
        {
            Lookup::Upgrade {
                registry,
                name,
                stable_only,
                ..
            } => (registry.as_deref(), name.as_str(), *stable_only),
            Lookup::AddToWorkspace {
                name, stable_only, ..
            } => (None, name.as_str(), *stable_only),
        };
        let versions = match self.info.get_info(registry, name).await
        {
            Ok(v) => v,
            Err(e) =>
            {
                self.client
                    .log_message(MessageType::WARNING, report(e.as_ref()))
                    .await;
                return None;
            }
        };
        let newest = suggestable(&versions, stable_only).next_back()?.version();
        match lookup
        {
            Lookup::Upgrade {
                uri,
                requirement,
                range,
                ..
            } =>
            {
                is_newer(&requirement, newest)
                    .then(|| replace_action("Upgrade", &uri, range, format!("\"{newest}\"")))
            }
            Lookup::AddToWorkspace { root, name, at, .. } =>
            {
                Some(replace_action(
                    "Add to workspace",
                    &root,
                    Range::new(at, at),
                    format!("\n{name} = \"{newest}\""),
                ))
            }
        }
    }
}
