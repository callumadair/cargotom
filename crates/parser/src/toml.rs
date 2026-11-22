//! The dependencies and features of a manifest, with the
//! byte range of each part for editor features.

use std::{
    path::Path,
    sync::Arc,
};

use cargo::{
    GlobalContext,
    workspace::{
        FeatureValue,
        Features,
        editor::dependency::{
            self as editor_dep,
            GitSource,
            PathSource,
            RegistrySource,
            WorkspaceSource,
        },
    },
};
use cargo_platform::Platform;
use cargo_util::paths::normalize_path;
use info_provider::Cause;
use snafu::{
    OptionExt,
    ResultExt,
    Snafu,
};
use toml_edit::{
    DocumentMut,
    Item,
    Value,
};

use crate::tree::ByteSpan;

/// The parsed parts of one manifest.
#[derive(Debug)]
pub struct Toml
{
    /// The manifest has a `[workspace]` table.
    pub workspace:    bool,
    /// Every dependency, from every dependency table and
    /// target.
    pub dependencies: Vec<Positioned<Dependency>>,
    /// The entries of `[features]`.
    pub features:     Vec<Positioned<Feature>>,
}

impl Toml
{
    /// Returns the dependencies and features of both, in
    /// order.
    pub fn join(
        self,
        other: Self,
    ) -> Self
    {
        Self {
            workspace:    self.workspace || other.workspace,
            dependencies: self
                .dependencies
                .into_iter()
                .chain(other.dependencies)
                .collect(),
            features:     self.features.into_iter().chain(other.features).collect(),
        }
    }
}

/// Byte offset of start and end of a value
#[derive(Debug, Clone, Copy)]
pub struct Positioned<T>
{
    /// Byte offset of the start of the value
    pub start: u32,
    /// Byte offset of the end of the value
    pub end:   u32,
    /// Value
    pub data:  T,
}

impl<T> Positioned<T>
{
    /// Returns `data` positioned from `start` to `end`.
    pub fn new(
        start: u32,
        end: u32,
        data: T,
    ) -> Self
    {
        Self { start, end, data }
    }

    /// Returns `true` when `offset` is between `start` and
    /// `end`, both included.
    pub fn touches(
        &self,
        offset: usize,
    ) -> bool
    {
        ByteSpan::from(self).touches(offset)
    }

    /// Returns `true` when `range` and the value share a
    /// byte, ends included.
    pub fn overlap(
        &self,
        range: ByteSpan,
    ) -> bool
    {
        range.start <= self.end && range.end >= self.start
    }
}

/// A dependency table. `FromStr` parses the table name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::EnumString)]
pub enum DependencyKind
{
    /// `[dependencies]`
    #[strum(serialize = "dependencies")]
    Normal,
    /// `[dev-dependencies]`
    #[strum(serialize = "dev-dependencies")]
    Development,
    /// `[build-dependencies]`
    #[strum(serialize = "build-dependencies")]
    Build,
}

/// One dependency entry, which can be incomplete while the
/// user types.
#[derive(Debug, Clone)]
pub struct Dependency
{
    /// Name of the dependency
    pub name:               Positioned<String>,
    /// Dev dependency or normal dependency
    pub kind:               DependencyKind,
    /// Source of the dependency
    pub source:             DepSource,
    /// The `package` key, which names the crate when it
    /// differs from the key
    pub package:            Option<Positioned<String>>,
    /// Enable features for this dependency
    pub features:           Positioned<Vec<Positioned<String>>>,
    /// The byte range of the `features` key
    pub features_key_range: Option<ByteSpan>,
    /// The `default-features` key
    pub default_features:   Option<Positioned<bool>>,
    /// Keys that are being typed
    pub typing_keys:        Vec<Positioned<String>>,
    /// Is optional dependency
    pub optional:           Option<Positioned<bool>>,
    /// The dependency is written as an inline table and not
    /// as a dotted key or a bare version.
    pub expanded:           bool,
    /// Target platforms for this dependency
    /// if empty = all platforms
    pub target:             Arc<Vec<Positioned<Platform>>>,
}

impl Dependency
{
    /// Returns the crate name, which is `package` when set
    /// and the key otherwise.
    pub fn crate_name(&self) -> String
    {
        self.package
            .as_ref()
            .map(|v| &v.data)
            .unwrap_or(&self.name.data)
            .to_owned()
    }

    /// Sets the key. The range stays the same.
    pub fn set_name(
        &mut self,
        name: String,
    )
    {
        self.name.data = name;
    }

    /// Returns the key.
    pub fn name(&self) -> &str { &self.name.data }

    /// Returns the byte range of the key.
    pub fn crate_name_range(&self) -> ByteSpan { ByteSpan::from(&self.name) }
}

impl Dependency
{
    /// Returns this dependency as `Cargo.toml` text, such
    /// as `serde = "1"`.
    ///
    /// `from` is the absolute directory of the manifest
    /// that holds the dependency now, and `to` is the
    /// absolute directory of the manifest that
    /// will hold the text. A path source is rewritten
    /// relative to `to`.
    ///
    /// An expanded dependency becomes an inline table. A
    /// collapsed one uses the short form that cargo picks,
    /// with a one-key table written as a dotted key such as
    /// `serde.workspace = true`. A dependency without a
    /// source becomes its bare name.
    ///
    /// `gctx` supplies the cargo config that cargo reads to
    /// write the dependency.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestTextError::Relative`] when `from`
    /// or `to` is relative, and another
    /// [`ManifestTextError`] when cargo cannot write the
    /// dependency.
    pub fn to_manifest_text(
        &self,
        gctx: &GlobalContext,
        from: &Path,
        to: &Path,
    ) -> Result<String, ManifestTextError>
    {
        // Cargo panics on a relative directory.
        if let Some(path) = [from, to].into_iter().find(|v| v.is_relative())
        {
            return RelativeSnafu { path }.fail();
        }
        let mut dep = match &self.package
        {
            Some(package) => editor_dep::Dependency::new(&package.data).set_rename(self.name()),
            None => editor_dep::Dependency::new(self.name()),
        };
        dep = match &self.source
        {
            DepSource::Version { value, registry } =>
            {
                let dep = dep.set_source(RegistrySource::new(&value.value.data));
                match registry
                {
                    Some(registry) => dep.set_registry(&registry.data),
                    None => dep,
                }
            }
            DepSource::Git {
                url,
                rev,
                tag,
                branch,
            } =>
            {
                let mut git =
                    GitSource::new(url.as_ref().map(|v| v.data.as_str()).unwrap_or_default());
                if let Some(branch) = branch
                {
                    git = git.set_branch(&branch.data);
                }
                if let Some(tag) = tag
                {
                    git = git.set_tag(&tag.data);
                }
                if let Some(rev) = rev
                {
                    git = git.set_rev(&rev.data);
                }
                dep.set_source(git)
            }
            // Cargo diffs the paths without resolving `..`,
            // so normalise first.
            DepSource::Path(path) =>
            {
                dep.set_source(PathSource::new(normalize_path(&from.join(&path.data))))
            }
            DepSource::Workspace(_) => dep.set_source(WorkspaceSource::new()),
            DepSource::None => return Ok(self.name().to_owned()),
        };
        if self.optional.is_some_and(|v| v.data)
        {
            dep = dep.set_optional(true);
        }
        if let Some(default_features) = self.default_features
        {
            dep = dep.set_default_features(default_features.data);
        }
        if !self.features.data.is_empty()
        {
            dep = dep.set_features(self.features.data.iter().map(|v| v.data.clone()).collect());
        }

        let name = self.name();
        let item = dep
            .to_toml(gctx, to, to, &Features::default())
            .map_err(Cause::from)
            .context(WriteSnafu { name })?;
        let value = item.into_value().ok().context(NotAValueSnafu { name })?;
        let item = match value
        {
            Value::String(version) if self.expanded =>
            {
                let mut table = toml_edit::InlineTable::new();
                table.insert("version", Value::String(version));
                Value::InlineTable(table)
            }
            Value::InlineTable(mut table) =>
            {
                table.set_dotted(!self.expanded && table.len() == 1);
                Value::InlineTable(table)
            }
            value => value,
        };
        let mut doc = DocumentMut::new();
        doc.insert(dep.toml_key(), Item::Value(item));
        Ok(doc.to_string().trim_end().to_owned())
    }
}

/// A dependency that cargo cannot write as manifest text.
#[derive(Debug, Snafu)]
pub enum ManifestTextError
{
    /// A manifest directory is relative.
    #[snafu(display("the manifest directory {} is not absolute", path.display()))]
    Relative
    {
        /// The directory.
        path: std::path::PathBuf,
    },
    /// Cargo rejected the dependency.
    #[snafu(display("cargo cannot write the dependency `{name}`"))]
    Write
    {
        /// The dependency key.
        name:   String,
        /// The cargo error.
        source: Cause,
    },
    /// Cargo wrote the dependency as a table section, not
    /// as a value.
    #[snafu(display("cargo wrote the dependency `{name}` as a table, not a value"))]
    NotAValue
    {
        /// The dependency key.
        name: String,
    },
}

/// A version requirement, with the range of its `version`
/// key when the dependency is a table.
#[derive(Debug, Clone)]
pub struct OptionalKey
{
    /// The byte range of the `version` key.
    pub key:   Option<ByteSpan>,
    /// The requirement.
    pub value: Positioned<String>,
}

impl OptionalKey
{
    /// Returns a requirement written as `version = "..."`.
    pub fn with_key(
        range: ByteSpan,
        value: Positioned<String>,
    ) -> Self
    {
        Self {
            key: Some(range),
            value,
        }
    }

    /// Returns a requirement written as a bare string.
    pub fn no_key(value: Positioned<String>) -> Self { Self { key: None, value } }
}

/// Where a dependency comes from.
///
/// A `set_*` method switches to the source kind that its
/// key belongs to and keeps the other keys of that kind.
/// Only a version leaves a git or path source unchanged.
#[derive(Debug, Clone)]
pub enum DepSource
{
    /// registry source
    Version
    {
        /// The version requirement.
        value:    OptionalKey,
        /// The `registry` key. `None` means crates.io.
        registry: Option<Positioned<String>>,
    },
    /// Git source. A key that the manifest leaves out is
    /// `None`.
    Git
    {
        /// `git`
        url:    Option<Positioned<String>>,
        /// `rev`
        rev:    Option<Positioned<String>>,
        /// `tag`
        tag:    Option<Positioned<String>>,
        /// `branch`
        branch: Option<Positioned<String>>,
    },
    /// Local source, with the path as written
    Path(Positioned<String>),
    /// Broken file
    /// for suggestions while typing the name
    None,
    /// `workspace = true`, with the byte range of the key
    /// and value.
    Workspace(ByteSpan),
}

impl DepSource
{
    /// Returns the `registry` key of a registry source.
    pub fn registry(&self) -> Option<&str>
    {
        match self
        {
            DepSource::Version { registry, .. } => registry.as_ref().map(|v| v.data.as_str()),
            _ => None,
        }
    }

    /// Returns the byte range of the version key and value,
    /// or of `workspace = true`.
    pub fn range(&self) -> Option<ByteSpan>
    {
        match self
        {
            DepSource::Version { value, .. } =>
            {
                let mut r = ByteSpan::from(&value.value);
                if let Some(v) = value.key
                {
                    r = r.join(&v)
                }
                Some(r)
            }
            Self::Workspace(r) => Some(*r),
            _ => None,
        }
    }

    /// Returns `true` when `offset` is on the version key
    /// or value, ends included.
    pub fn touches(
        &self,
        offset: usize,
    ) -> bool
    {
        match self
        {
            DepSource::Version { value, .. } =>
            {
                value.value.touches(offset) || value.key.is_some_and(|v| v.touches(offset))
            }
            _ => false,
        }
    }

    /// Returns the byte offset of the end of the version
    /// value.
    pub fn end(&self) -> Option<u32>
    {
        match self
        {
            DepSource::Version { value, .. } => Some(value.value.end),
            _ => None,
        }
    }

    /// Returns the version requirement of a registry
    /// source.
    pub fn version(&self) -> Option<&Positioned<String>>
    {
        match self
        {
            DepSource::Version { value, .. } => Some(&value.value),
            _ => None,
        }
    }

    /// Sets `workspace = true`.
    pub fn set_workspace(
        &mut self,
        range: ByteSpan,
    )
    {
        *self = DepSource::Workspace(range)
    }

    /// Sets the version requirement.
    pub fn set_version(
        &mut self,
        key: OptionalKey,
    )
    {
        match self
        {
            DepSource::Version { value, .. } =>
            {
                *value = key;
            }
            DepSource::Git { .. } | DepSource::Path(_) =>
            {}
            _ =>
            {
                *self = DepSource::Version {
                    value:    key,
                    registry: None,
                }
            }
        }
    }

    /// Sets the `git` URL.
    pub fn set_git(
        &mut self,
        key: Positioned<String>,
    )
    {
        match self
        {
            DepSource::Git { url, .. } =>
            {
                *url = Some(key);
            }
            _ =>
            {
                *self = DepSource::Git {
                    url:    Some(key),
                    rev:    None,
                    tag:    None,
                    branch: None,
                }
            }
        }
    }

    /// Sets the git `branch`.
    pub fn set_branch(
        &mut self,
        key: Positioned<String>,
    )
    {
        match self
        {
            DepSource::Git { branch, .. } =>
            {
                *branch = Some(key);
            }
            _ =>
            {
                *self = DepSource::Git {
                    url:    None,
                    rev:    None,
                    tag:    None,
                    branch: Some(key),
                }
            }
        }
    }

    /// Sets the git `rev`.
    pub fn set_rev(
        &mut self,
        key: Positioned<String>,
    )
    {
        match self
        {
            DepSource::Git { rev, .. } =>
            {
                *rev = Some(key);
            }
            _ =>
            {
                *self = DepSource::Git {
                    url:    None,
                    rev:    Some(key),
                    tag:    None,
                    branch: None,
                }
            }
        }
    }

    /// Sets the git `tag`.
    pub fn set_tag(
        &mut self,
        key: Positioned<String>,
    )
    {
        match self
        {
            DepSource::Git { tag, .. } =>
            {
                *tag = Some(key);
            }
            _ =>
            {
                *self = DepSource::Git {
                    url:    None,
                    rev:    None,
                    tag:    Some(key),
                    branch: None,
                }
            }
        }
    }

    /// Sets the `path`.
    pub fn set_path(
        &mut self,
        key: Positioned<String>,
    )
    {
        match self
        {
            DepSource::Path(value) =>
            {
                *value = key;
            }
            _ =>
            {
                *self = DepSource::Path(key);
            }
        }
    }

    /// Sets the `registry`.
    pub fn set_registry(
        &mut self,
        key: Positioned<String>,
    )
    {
        match self
        {
            DepSource::Version { registry, .. } =>
            {
                *registry = Some(key);
            }
            _ =>
            {
                *self = DepSource::Version {
                    value:    OptionalKey {
                        key:   None,
                        value: Positioned {
                            start: 0,
                            end:   0,
                            data:  String::new(),
                        },
                    },
                    registry: Some(key),
                }
            }
        }
    }
}

/// One entry of `[features]`.
#[derive(Debug)]
pub struct Feature
{
    /// feature-name =
    pub name: Positioned<String>,
    /// = [...]
    pub args: Vec<Positioned<FeatureValue>>,
}

#[cfg(test)]
mod tests
{
    use std::{
        path::Path,
        sync::Arc,
    };

    use cargo::GlobalContext;
    use cargo_util_terminal::Shell;
    use rstest::rstest;

    use super::{
        Dependency,
        ManifestTextError,
    };
    use crate::{
        tree::Tree,
        tree_to_struct::to_struct,
    };

    type BoxError = Box<dyn std::error::Error>;

    fn first_dependency(manifest: &str) -> Result<Dependency, BoxError>
    {
        let dom = taplo::parser::parse(manifest).into_dom();
        let tree = Tree::from(dom.as_table().ok_or("the document root is not a table")?);
        let dependency = to_struct(&tree, Arc::default())
            .dependencies
            .into_iter()
            .next()
            .ok_or("the manifest has no dependency")?;
        Ok(dependency.data)
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
    fn to_manifest_text(
        #[case] entry: &str,
        #[case] expected: &str,
    ) -> Result<(), BoxError>
    {
        let home = tempfile::tempdir()?;
        let dep = first_dependency(&format!("[dependencies]\n{entry}\n"))?;
        let dir = Path::new("/workspace/member");
        let text = dep.to_manifest_text(&gctx(home.path()), dir, dir)?;
        assert_eq!(text, expected);
        Ok(())
    }

    #[test]
    fn to_manifest_text_rewrites_path_relative_to_target() -> Result<(), BoxError>
    {
        let home = tempfile::tempdir()?;
        let dep = first_dependency("[dependencies]\nfoo = { path = \"../foo\" }\n")?;
        let text = dep.to_manifest_text(
            &gctx(home.path()),
            Path::new("/workspace/member"),
            Path::new("/workspace"),
        )?;
        assert_eq!(text, r#"foo = { path = "foo" }"#);
        Ok(())
    }

    #[test]
    fn to_manifest_text_rejects_a_relative_directory() -> Result<(), BoxError>
    {
        let home = tempfile::tempdir()?;
        let dep = first_dependency("[dependencies]\nfoo = { path = \"../foo\" }\n")?;
        let dir = Path::new("member");
        let text = dep.to_manifest_text(&gctx(home.path()), dir, dir);
        assert!(matches!(text, Err(ManifestTextError::Relative { path }) if path == dir));
        Ok(())
    }

    /// Returns a cargo context that reads no config outside
    /// `root`.
    fn gctx(root: &Path) -> GlobalContext
    {
        GlobalContext::new(Shell::new(), root.to_path_buf(), root.join("cargo-home"))
    }
}
