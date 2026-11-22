use cargo_util_schemas::core::{
    PartialVersion,
    SourceKind,
};
use info_provider::api::{
    ViewMode,
    feature_lines,
};
use parser::{
    Db,
    static_structure,
    toml::{
        DepSource,
        Dependency,
        Positioned,
    },
    tree::ByteSpan,
};
use tower_lsp::lsp_types::{
    Hover,
    HoverContents,
    MarkupContent,
    MarkupKind,
    MessageType,
    Position,
    Range,
    Url,
};

use crate::{
    lsp::Context,
    utils::report,
};

/// Index URL that `Cargo.lock` records for a crates.io
/// package.
const CRATES_IO_INDEX: &str = "https://github.com/rust-lang/crates.io-index";

/// The part of a dependency under the cursor, with the data
/// that its hover fetches. The handler gathers it under the
/// db lock and fetches without it.
enum Target
{
    /// The version requirement lists every published
    /// version.
    Versions
    {
        registry: Option<String>,
        name:     String,
    },
    /// The crate name shows the readme of the locked
    /// crates.io version.
    Readme
    {
        name: String, version: String
    },
    /// The feature list shows the features of the matching
    /// version.
    Features
    {
        registry:    Option<String>,
        name:        String,
        requirement: PartialVersion,
        mode:        ViewMode,
    },
}

impl Context
{
    /// Returns the hover for the dependency part or the
    /// schema key at `position`.
    pub async fn hover(
        &self,
        uri: &Url,
        position: Position,
    ) -> Option<Hover>
    {
        let (target, range) = {
            let db = self.db.read().await;
            match dependency_target(&db, uri, position)
            {
                Some(found) => found,
                None => return key_hover(&db, uri, position),
            }
        };
        let (kind, value) = match target
        {
            Target::Versions { registry, name } =>
            {
                let versions = self.info.get_info(registry.as_deref(), &name).await;
                let value = match versions
                {
                    Ok(v) =>
                    {
                        format!(
                            "List of all available versions: \n{}",
                            lines(v.iter().rev().map(|v| v.version().to_string()))
                        )
                    }
                    Err(e) => format!("Could not find version info: {}", report(e.as_ref())),
                };
                (MarkupKind::Markdown, value)
            }
            Target::Readme { name, version } =>
            {
                match self.info.get_readme(&name, &version).await
                {
                    Ok(v) => (MarkupKind::Markdown, v),
                    Err(e) =>
                    {
                        self.client
                            .log_message(MessageType::WARNING, report(e.as_ref()))
                            .await;
                        return None;
                    }
                }
            }
            Target::Features {
                registry,
                name,
                requirement,
                mode,
            } =>
            {
                let versions = self.info.get_info(registry.as_deref(), &name).await;
                let value = match versions
                {
                    Ok(v) =>
                    {
                        format!(
                            "List of all available features: \n{}",
                            lines(
                                v.iter()
                                    .rfind(|v| requirement.matches(v.version()))
                                    .map(|v| feature_lines(v, mode))
                                    .unwrap_or_default()
                            )
                        )
                    }
                    Err(e) => format!("Could not find feature info: {}", report(e.as_ref())),
                };
                (MarkupKind::Markdown, value)
            }
        };
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent { kind, value }),
            range:    Some(range),
        })
    }
}

/// Formats each item as one Markdown list entry.
fn lines(items: impl IntoIterator<Item = String>) -> String
{
    items
        .into_iter()
        .map(|v| format!("- {v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Returns the hover target of the dependency at
/// `position`, and the range that the hover covers.
fn dependency_target(
    db: &Db,
    uri: &Url,
    position: Position,
) -> Option<(Target, Range)>
{
    let next = Position::new(position.line, position.character.saturating_add(1));
    let dep = db.get_dependency(uri, Range::new(position, next))?;
    let offset = db.get_byte(uri, position)?;
    let data = &dep.data;
    let registry = data.source.registry().map(ToOwned::to_owned);

    if let Some(span) = data.source.range().filter(|v| v.touches(offset))
    {
        let target = Target::Versions {
            registry,
            name: data.crate_name(),
        };
        return Some((target, db.get_range(uri, span)?));
    }
    let name_span = data.crate_name_range();
    if name_span.touches(offset)
    {
        let locked = db.get_locked_package(uri, data.name())?;
        let source = locked.source.as_ref()?;
        if !matches!(source.kind(), SourceKind::Registry)
            || source.url().as_str() != CRATES_IO_INDEX
        {
            return None;
        }
        let target = Target::Readme {
            name:    data.crate_name(),
            version: locked.version.to_string(),
        };
        return Some((target, db.get_range(uri, name_span)?));
    }
    let features_span = ByteSpan::from(&data.features);
    if dep.end == 0 || !features_span.touches(offset)
    {
        return None;
    }
    let target = Target::Features {
        registry,
        name: data.crate_name(),
        requirement: requirement(db, uri, dep)?.parse().ok()?,
        mode: db.config.feature_display_mode,
    };
    Some((target, db.get_range(uri, features_span)?))
}

/// Returns the version requirement of `dep`, read from the
/// workspace root for a `workspace = true` dependency.
fn requirement<'a>(
    db: &'a Db,
    uri: &Url,
    dep: &'a Positioned<Dependency>,
) -> Option<&'a str>
{
    match &dep.data.source
    {
        DepSource::Version { value, .. } => Some(&value.value.data),
        DepSource::Workspace(_) =>
        {
            let root = db.get_toml(db.get_workspace(uri)?)?;
            let name = dep.data.crate_name();
            let root_dep = root.dependencies.iter().find(|v| v.data.name() == name)?;
            Some(&root_dep.data.source.version()?.data)
        }
        _ => None,
    }
}

/// Returns the schema description of the key or value at
/// `position`.
fn key_hover(
    db: &Db,
    uri: &Url,
    position: Position,
) -> Option<Hover>
{
    let path = db.get_path(uri, position)?;
    let last = path.last()?;
    let keys: Vec<String> = path.iter().map(|v| v.ty.to_string()).collect();
    // `initialize` reports a schema that does not parse.
    let detail = static_structure::schema()
        .ok()?
        .get_detail(&keys, 0, last.is_value(None))?;
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind:  MarkupKind::PlainText,
            value: detail,
        }),
        range:    Some(db.get_range(uri, last.range)?),
    })
}
