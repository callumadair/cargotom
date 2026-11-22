use std::{path::Path, sync::Arc};

use cargo::{
    GlobalContext,
    workspace::{
        Features,
        FeatureValue,
        editor::dependency::{
            self as editor_dep, GitSource, PathSource, RegistrySource, WorkspaceSource,
        },
    },
};
use cargo_platform::Platform;
use cargo_util::paths::normalize_path;
use toml_edit::{DocumentMut, Item, Value};

use crate::tree::RangeExclusive;

#[derive(Debug)]
pub struct Toml {
    pub workspace: bool,
    pub dependencies: Vec<Positioned<Dependency>>,
    pub features: Vec<Positioned<Feature>>,
}

impl Toml {
    pub fn join(self, other: Self) -> Self {
        Self {
            workspace: self.workspace || other.workspace,
            dependencies: self
                .dependencies
                .into_iter()
                .chain(other.dependencies)
                .collect(),
            features: self.features.into_iter().chain(other.features).collect(),
        }
    }
}

/// Byte offset of start and end of a value
#[derive(Debug, Clone, Copy)]
pub struct Positioned<T> {
    /// Byte offset of the start of the value
    pub start: u32,
    /// Byte offset of the end of the value
    pub end: u32,
    /// Value
    pub data: T,
}

impl<T> Positioned<T> {
    pub fn new(start: u32, end: u32, data: T) -> Self {
        Self { start, end, data }
    }

    pub fn contains(&self, offset: usize) -> bool {
        self.start <= offset as u32 && offset as u32 <= self.end
    }

    pub fn overlap(&self, range: RangeExclusive) -> bool {
        range.start <= self.end && range.end >= self.start
    }
}

/// A dependency table. `FromStr` parses the table name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::EnumString)]
pub enum DependencyKind {
    #[strum(serialize = "dependencies")]
    Normal,
    #[strum(serialize = "dev-dependencies")]
    Development,
    #[strum(serialize = "build-dependencies")]
    Build,
}

#[derive(Debug, Clone)]
pub struct Dependency {
    /// Name of the dependency
    pub name: Positioned<String>,
    /// Dev dependency or normal dependency
    pub kind: DependencyKind,
    /// Source of the dependency
    pub source: DepSource,
    pub package: Option<Positioned<String>>,
    /// Enable features for this dependency
    pub features: Positioned<Vec<Positioned<String>>>,
    pub features_key_range: Option<RangeExclusive>,
    pub default_features: Option<Positioned<bool>>,
    /// Keys that are being typed
    pub typing_keys: Vec<Positioned<String>>,
    /// Is optional dependency
    pub optional: Option<Positioned<bool>>,
    pub expanded: bool,
    /// Target platforms for this dependency
    /// if empty = all platforms
    pub target: Arc<Vec<Positioned<Platform>>>,
}

impl Dependency {
    pub fn crate_name(&self) -> String {
        self.package
            .as_ref()
            .map(|v| &v.data)
            .unwrap_or(&self.name.data)
            .to_owned()
    }

    pub fn set_name(&mut self, name: String) {
        self.name.data = name;
    }

    pub fn name(&self) -> &str {
        &self.name.data
    }

    pub fn crate_name_range(&self) -> RangeExclusive {
        RangeExclusive::from(&self.name)
    }
}

impl Dependency {
    /// Returns this dependency as `Cargo.toml` text, such as `serde = "1"`.
    ///
    /// `from` is the absolute directory of the manifest that holds the
    /// dependency now, and `to` is the absolute directory of the manifest that
    /// will hold the text. A path source is rewritten relative to `to`.
    ///
    /// An expanded dependency becomes an inline table. A collapsed one uses
    /// the short form that cargo picks, with a one-key table written as a
    /// dotted key such as `serde.workspace = true`. A dependency without a
    /// source becomes its bare name.
    ///
    /// Returns `None` when cargo cannot write the dependency, for example when
    /// `from` or `to` is relative.
    pub fn to_manifest_text(&self, from: &Path, to: &Path) -> Option<String> {
        let mut dep = match &self.package {
            Some(package) => editor_dep::Dependency::new(&package.data).set_rename(self.name()),
            None => editor_dep::Dependency::new(self.name()),
        };
        dep = match &self.source {
            DepSource::Version { value, registry } => {
                let dep = dep.set_source(RegistrySource::new(&value.value.data));
                match registry {
                    Some(registry) => dep.set_registry(&registry.value.data),
                    None => dep,
                }
            }
            DepSource::Git {
                url,
                rev,
                tag,
                branch,
            } => {
                let mut git = GitSource::new(url.as_ref().map(|v| v.value.data.as_str()).unwrap_or_default());
                if let Some(branch) = branch {
                    git = git.set_branch(&branch.value.data);
                }
                if let Some(tag) = tag {
                    git = git.set_tag(&tag.value.data);
                }
                if let Some(rev) = rev {
                    git = git.set_rev(&rev.value.data);
                }
                dep.set_source(git)
            }
            // Cargo diffs the paths without resolving `..`, so normalise first.
            DepSource::Path(path) => dep.set_source(PathSource::new(normalize_path(
                &from.join(&path.value.data),
            ))),
            DepSource::Workspace(_) => dep.set_source(WorkspaceSource::new()),
            DepSource::None => return Some(self.name().to_owned()),
        };
        if self.optional.is_some_and(|v| v.data) {
            dep = dep.set_optional(true);
        }
        if let Some(default_features) = self.default_features {
            dep = dep.set_default_features(default_features.data);
        }
        if !self.features.data.is_empty() {
            dep = dep.set_features(self.features.data.iter().map(|v| v.data.clone()).collect());
        }

        let gctx = GlobalContext::default().ok()?;
        let item = dep.to_toml(&gctx, to, to, &Features::default()).ok()?;
        let item = match item.into_value().ok()? {
            Value::String(version) if self.expanded => {
                let mut table = toml_edit::InlineTable::new();
                table.insert("version", Value::String(version));
                Value::InlineTable(table)
            }
            Value::InlineTable(mut table) => {
                table.set_dotted(!self.expanded && table.len() == 1);
                Value::InlineTable(table)
            }
            value => value,
        };
        let mut doc = DocumentMut::new();
        doc.insert(dep.toml_key(), Item::Value(item));
        Some(doc.to_string().trim_end().to_owned())
    }
}

#[derive(Debug, Clone)]
pub struct WithKey {
    key: RangeExclusive,
    pub value: Positioned<String>,
}

impl WithKey {
    pub fn new(range: RangeExclusive, value: Positioned<String>) -> Self {
        Self { key: range, value }
    }
}

#[derive(Debug, Clone)]
pub struct OptionalKey {
    pub key: Option<RangeExclusive>,
    pub value: Positioned<String>,
}

impl OptionalKey {
    pub fn with_key(range: RangeExclusive, value: Positioned<String>) -> Self {
        Self {
            key: Some(range),
            value,
        }
    }
    pub fn no_key(value: Positioned<String>) -> Self {
        Self { key: None, value }
    }
}

#[derive(Debug, Clone)]
pub enum DepSource {
    /// registry source
    Version {
        value: OptionalKey,
        registry: Option<WithKey>,
    },
    /// Git source
    Git {
        url: Option<WithKey>,
        rev: Option<WithKey>,
        tag: Option<WithKey>,
        branch: Option<WithKey>,
    },
    /// Local source
    Path(WithKey),
    /// Broken file
    /// for suggestions while typing the name
    None,
    Workspace(RangeExclusive),
}

impl DepSource {
    pub fn registry(&self) -> Option<&str> {
        match self {
            DepSource::Version { registry, .. } => registry.as_ref().map(|v| v.value.data.as_str()),
            _ => None,
        }
    }
    pub fn range(&self) -> Option<RangeExclusive> {
        match self {
            DepSource::Version { value, .. } => {
                let mut r = RangeExclusive::from(&value.value);
                if let Some(v) = value.key {
                    r = r.join(&v)
                }
                Some(r)
            }
            Self::Workspace(r) => Some(r.clone()),
            _ => None,
        }
    }
    pub fn contains(&self, offset: usize) -> bool {
        match self {
            DepSource::Version { value, registry } => {
                value.value.contains(offset)
                    || value.key.map(|v| v.contains(offset)).unwrap_or_default()
            }
            _ => false,
        }
    }
    pub fn end(&self) -> Option<u32> {
        match self {
            DepSource::Version { value, .. } => Some(value.value.end),
            _ => None,
        }
    }

    pub fn version(&self) -> Option<&Positioned<String>> {
        match self {
            DepSource::Version { value, .. } => Some(&value.value),
            _ => None,
        }
    }

    pub fn set_workspace(&mut self, range: RangeExclusive) {
        *self = DepSource::Workspace(range)
    }
    pub fn set_version(&mut self, key: OptionalKey) {
        match self {
            DepSource::Version { value, .. } => {
                *value = key;
            }
            DepSource::Git { .. } | DepSource::Path(_) => {}
            _ => {
                *self = DepSource::Version {
                    value: key,
                    registry: None,
                }
            }
        }
    }

    pub fn set_git(&mut self, key: WithKey) {
        match self {
            DepSource::Git { url, .. } => {
                *url = Some(key);
            }
            _ => {
                *self = DepSource::Git {
                    url: Some(key),
                    rev: None,
                    tag: None,
                    branch: None,
                }
            }
        }
    }

    pub fn set_branch(&mut self, key: WithKey) {
        match self {
            DepSource::Git { branch, .. } => {
                *branch = Some(key);
            }
            _ => {
                *self = DepSource::Git {
                    url: None,
                    rev: None,
                    tag: None,
                    branch: Some(key),
                }
            }
        }
    }

    pub fn set_rev(&mut self, key: WithKey) {
        match self {
            DepSource::Git { rev, .. } => {
                *rev = Some(key);
            }
            _ => {
                *self = DepSource::Git {
                    url: None,
                    rev: Some(key),
                    tag: None,
                    branch: None,
                }
            }
        }
    }

    pub fn set_tag(&mut self, key: WithKey) {
        match self {
            DepSource::Git { tag, .. } => {
                *tag = Some(key);
            }
            _ => {
                *self = DepSource::Git {
                    url: None,
                    rev: None,
                    tag: Some(key),
                    branch: None,
                }
            }
        }
    }

    pub fn set_path(&mut self, key: WithKey) {
        match self {
            DepSource::Path(value) => {
                *value = key;
            }
            _ => {
                *self = DepSource::Path(key);
            }
        }
    }

    pub fn set_registry(&mut self, key: WithKey) {
        match self {
            DepSource::Version { registry, .. } => {
                *registry = Some(key);
            }
            _ => {
                *self = DepSource::Version {
                    value: OptionalKey {
                        key: None,
                        value: Positioned {
                            start: 0,
                            end: 0,
                            data: String::new(),
                        },
                    },
                    registry: Some(key),
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct Feature {
    /// feature-name =
    pub name: Positioned<String>,
    /// = [...]
    pub args: Vec<Positioned<FeatureValue>>,
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use rstest::rstest;

    use super::Dependency;
    use crate::{tree::Tree, tree_to_struct::to_struct};

    fn first_dependency(manifest: &str) -> Dependency {
        let dom = taplo::parser::parse(manifest).into_dom();
        let tree = Tree::from(dom.as_table().unwrap());
        to_struct(&tree, Arc::default()).dependencies.remove(0).data
    }

    #[rstest]
    #[case::bare_name("serde", "serde")]
    #[case::version_string(r#"serde = "1""#, r#"serde = "1""#)]
    #[case::expanded_version(r#"serde = { version = "1" }"#, r#"serde = { version = "1" }"#)]
    #[case::collapsed_workspace("serde.workspace = true", "serde.workspace = true")]
    #[case::expanded_workspace("serde = { workspace = true }", "serde = { workspace = true }")]
    #[case::rename(
        r#"json = { package = "serde_json", version = "1" }"#,
        r#"json = { version = "1", package = "serde_json" }"#
    )]
    #[case::features_and_optional(
        r#"serde = { version = "1", features = ["derive"], optional = true }"#,
        r#"serde = { version = "1", features = ["derive"], optional = true }"#
    )]
    #[case::optional_false_dropped(
        r#"serde = { version = "1", optional = false }"#,
        r#"serde = { version = "1" }"#
    )]
    #[case::git_branch(
        r#"foo = { git = "https://example.com/foo", branch = "main" }"#,
        r#"foo = { git = "https://example.com/foo", branch = "main" }"#
    )]
    fn to_manifest_text(#[case] entry: &str, #[case] expected: &str) {
        let dep = first_dependency(&format!("[dependencies]\n{entry}\n"));
        let dir = Path::new("/workspace/member");
        assert_eq!(dep.to_manifest_text(dir, dir).as_deref(), Some(expected));
    }

    #[test]
    fn to_manifest_text_rewrites_path_relative_to_target() {
        let dep = first_dependency("[dependencies]\nfoo = { path = \"../foo\" }\n");
        let text = dep.to_manifest_text(Path::new("/workspace/member"), Path::new("/workspace"));
        assert_eq!(text.as_deref(), Some(r#"foo = { path = "foo" }"#));
    }
}
