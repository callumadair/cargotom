//! Document state and analysis for the cargotom language
//! server.
//!
//! [`Db`] holds the open manifests, their syntax trees, the
//! parsed dependencies and features, and the `Cargo.lock`
//! files beside them.

mod analyze;
pub mod config;
mod format;
pub mod static_structure;
pub mod structs;
pub mod toml;
pub mod tree;
mod tree_to_struct;

use std::{
    collections::HashMap,
    io,
    path::{
        Path,
        PathBuf,
    },
    sync::{
        Arc,
        Weak,
    },
};

use cargo_util_schemas::lockfile::{
    TomlLockfile,
    TomlLockfileDependency,
};
use config::Config;
use info_provider::{
    Cause,
    InfoError,
    InfoProvider,
};
use ropey::Rope;
use snafu::{
    OptionExt,
    ResultExt,
    Snafu,
};
use structs::lock::LockedPackage;
use tokio::sync::RwLock;
use toml::{
    Dependency,
    Positioned,
    Toml,
};
use tower_lsp::{
    Client,
    lsp_types::{
        MessageType,
        Position,
        Range,
    },
};
use tree::{
    ByteSpan,
    PathValue,
    Tree,
};
use tree_to_struct::to_struct;
use url::Url;

/// The address of a document, as the client sends it.
pub type Uri = url::Url;

/// A failed document operation of [`Db`].
#[derive(Debug, Snafu)]
pub enum DbError
{
    /// The document is not open.
    #[snafu(display("the document {uri} is not open"))]
    NotOpen
    {
        /// The document.
        uri: Uri,
    },
    /// A position is past the end of the document.
    #[snafu(display("a position is outside the document {uri}"))]
    Position
    {
        /// The document.
        uri: Uri,
    },
    /// The address is not a local file.
    #[snafu(display("the document {uri} is not a local file"))]
    NotAFile
    {
        /// The document.
        uri: Uri,
    },
    /// A file cannot be read.
    #[snafu(display("cannot read {}", path.display()))]
    Read
    {
        /// The file.
        path:   PathBuf,
        /// The I/O error.
        source: io::Error,
    },
    /// A `Cargo.lock` file is not a valid lock file.
    #[snafu(display("{} is not a valid lock file", path.display()))]
    LockParse
    {
        /// The file.
        path:   PathBuf,
        /// The TOML error.
        source: ::toml::de::Error,
    },
    /// Cargo cannot set up its config.
    #[snafu(display("cannot set up cargo"))]
    Context
    {
        /// The cargo error.
        source: InfoError,
    },
    /// Cargo cannot read a manifest above the document to
    /// find its workspace.
    #[snafu(display("cannot find the workspace of {}", path.display()))]
    WorkspaceRoot
    {
        /// The manifest.
        path:   PathBuf,
        /// The cargo error.
        source: Cause,
    },
}

/// The state of every manifest that the server knows.
///
/// A [`Position`] counts characters, not UTF-16 code units,
/// from zero. A byte offset counts UTF-8 bytes from the
/// start of the document.
pub struct Db
{
    /// A handle to the shared `Db` itself, so that a
    /// spawned lookup can analyse a document again when it
    /// ends.
    this:       Weak<RwLock<Db>>,
    /// The client that receives logs, diagnostics and
    /// refresh requests.
    pub client: Client,
    files:      HashMap<Uri, Rope>,
    trees:      HashMap<Uri, Tree>,
    tomls:      HashMap<Uri, Toml>,
    info:       Arc<InfoProvider>,
    /// Workspace root manifest of each member manifest.
    workspaces: HashMap<Uri, Uri>,
    locks:      HashMap<Uri, TomlLockfile>,
    /// The client settings.
    pub config: Config,
}

/// One level of indentation. `Display` writes the indent
/// string.
#[derive(derive_more::Display)]
pub enum Indent
{
    /// This many spaces.
    #[display("{}", " ".repeat(*_0 as usize))]
    Spaces(u32),
    /// One tab.
    #[display("\t")]
    Tab,
}

impl Db
{
    /// Returns an empty `Db` with the default [`Config`].
    pub fn new(
        client: Client,
        info: Arc<InfoProvider>,
    ) -> Arc<RwLock<Db>>
    {
        Arc::new_cyclic(|this| {
            RwLock::new(Self {
                this: this.clone(),
                client,
                files: HashMap::new(),
                trees: HashMap::new(),
                tomls: HashMap::new(),
                info,
                workspaces: HashMap::new(),
                locks: HashMap::new(),
                config: Config::default(),
            })
        })
    }

    /// Returns the root manifest of the workspace that
    /// lists `manifest` as a member, read from disk by
    /// cargo. Returns `None` for a workspace root and for a
    /// package outside any workspace.
    fn find_workspace_root(
        &self,
        manifest: &Uri,
    ) -> Result<Option<Uri>, DbError>
    {
        let path = file_path(manifest)?;
        let gctx = self.info.context().context(ContextSnafu)?;
        let root = cargo::workspace::find_workspace_root(&path, &gctx)
            .map_err(Cause::from)
            .context(WorkspaceRootSnafu { path })?;
        Ok(root.and_then(|root| Url::from_file_path(root).ok()))
    }

    /// Returns the keys and the value under the cursor,
    /// outermost first. Returns `None` for an unknown
    /// document or when no key or value holds the cursor.
    pub fn get_path(
        &self,
        uri: &Uri,
        position: Position,
    ) -> Option<Vec<PathValue>>
    {
        let byte = self.get_byte(uri, position)?;
        let path = self.trees.get(uri)?.path(byte);
        (!path.is_empty()).then_some(path)
    }

    /// Forgets every document in the folder of
    /// `workspace_uri`.
    pub fn remove_workspace(
        &mut self,
        workspace_uri: &Url,
    )
    {
        let Ok(folder) = workspace_uri.to_file_path()
        else
        {
            return;
        };
        let outside = |uri: &Uri| !uri.to_file_path().is_ok_and(|v| v.starts_with(&folder));
        self.files.retain(|uri, _| outside(uri));
        self.trees.retain(|uri, _| outside(uri));
        self.tomls.retain(|uri, _| outside(uri));
    }

    /// Returns the first `Cargo.lock` entry named `name`
    /// for the workspace that holds `uri`.
    pub fn get_locked_package(
        &self,
        uri: &Uri,
        name: &str,
    ) -> Option<&TomlLockfileDependency>
    {
        let root = self.workspaces.get(uri).unwrap_or(uri);
        let lock = lock_uri(&root.to_file_path().ok()?)?;
        self.locks
            .get(&lock)?
            .package
            .as_ref()?
            .iter()
            .find(|package| package.name == name)
    }

    /// Returns the locked package of each dependency that
    /// `Cargo.lock` lists, with the position of the end of
    /// the dependency. Returns `None` for an
    /// unknown document.
    pub fn hints(
        &self,
        uri: &Uri,
    ) -> Option<Vec<(Position, LockedPackage<'_>)>>
    {
        let toml = self.tomls.get(uri)?;
        let hints = toml
            .dependencies
            .iter()
            .filter_map(|v| {
                let pos = self.get_offset(uri, v.end as usize)?;
                let package = self.get_locked_package(uri, &v.data.crate_name())?;
                Some((pos, LockedPackage(package)))
            })
            .collect();
        Some(hints)
    }

    /// Returns the current text of the document.
    pub fn get_content(
        &self,
        uri: &Uri,
    ) -> Option<String>
    {
        Some(self.files.get(uri)?.to_string())
    }

    /// Returns the workspace root manifest of a member
    /// manifest.
    pub fn get_workspace(
        &self,
        uri: &Uri,
    ) -> Option<&Uri>
    {
        self.workspaces.get(uri)
    }

    /// Returns the parsed dependencies and features of the
    /// document.
    pub fn get_toml(
        &self,
        uri: &Uri,
    ) -> Option<&Toml>
    {
        self.tomls.get(uri)
    }

    /// Converts a position to a byte offset. Returns `None`
    /// for an unknown document or a position past its end.
    pub fn get_byte(
        &self,
        uri: &Uri,
        position: Position,
    ) -> Option<usize>
    {
        let file = self.files.get(uri)?;
        file.try_char_to_byte(char_index(file, position)?).ok()
    }

    /// Converts a byte offset to a position. Returns `None`
    /// for an unknown document or an offset past its end.
    pub fn get_offset(
        &self,
        uri: &Uri,
        byte_offset: usize,
    ) -> Option<Position>
    {
        let file = self.files.get(uri)?;
        let line = file.try_byte_to_line(byte_offset).ok()?;
        let char = file.try_byte_to_char(byte_offset).ok()? - file.try_line_to_char(line).ok()?;
        Some(Position::new(
            u32::try_from(line).ok()?,
            u32::try_from(char).ok()?,
        ))
    }

    /// Converts a byte span to a range. Returns `None` for
    /// an unknown document or a span past its end.
    pub fn get_range(
        &self,
        uri: &Uri,
        span: ByteSpan,
    ) -> Option<Range>
    {
        Some(Range::new(
            self.get_offset(uri, span.start as usize)?,
            self.get_offset(uri, span.end as usize)?,
        ))
    }

    /// Returns the range from the start to the end of the
    /// document.
    pub fn full_range(
        &self,
        uri: &Uri,
    ) -> Option<Range>
    {
        let len = self.files.get(uri)?.len_bytes();
        Some(Range::new(Position::default(), self.get_offset(uri, len)?))
    }

    /// Reads the `Cargo.lock` at `uri` from disk and
    /// analyses every open document. A lock file that does
    /// not exist is forgotten.
    ///
    /// # Errors
    ///
    /// Returns [`DbError`] when the lock file cannot be
    /// read or parsed. The last good copy stays in use.
    pub async fn update_lock(
        &mut self,
        uri: Uri,
    ) -> Result<(), DbError>
    {
        let path = file_path(&uri)?;
        match tokio::fs::read_to_string(&path).await
        {
            Ok(text) =>
            {
                let lock = ::toml::from_str(&text).context(LockParseSnafu { path })?;
                self.locks.insert(uri, lock);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound =>
            {
                self.locks.remove(&uri);
            }
            Err(source) => return Err(DbError::Read { path, source }),
        }
        self.analyze(None).await;
        Ok(())
    }

    /// Returns the first dependency that overlaps `range`.
    pub fn get_dependency(
        &self,
        uri: &Uri,
        range: Range,
    ) -> Option<&Positioned<Dependency>>
    {
        let start = u32::try_from(self.get_byte(uri, range.start)?).ok()?;
        let end = u32::try_from(self.get_byte(uri, range.end)?).ok()?;
        self.tomls
            .get(uri)?
            .dependencies
            .iter()
            .find(|v| v.overlap(ByteSpan::new(start, end)))
    }

    /// Parses the current text of the document again and
    /// analyses it. A change to a workspace root analyses
    /// every open document.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::NotOpen`] when the document is
    /// not open.
    pub async fn reload(
        &mut self,
        uri: Uri,
    ) -> Result<(), DbError>
    {
        let content = self
            .files
            .get(&uri)
            .context(NotOpenSnafu { uri: uri.clone() })?;
        // The taplo DOM is not `Send`, so it must drop
        // before the await.
        let tree = taplo::parser::parse(&content.to_string())
            .into_dom()
            .as_table()
            .map(Tree::from);
        if let Some(tree) = tree
        {
            self.tomls
                .insert(uri.clone(), to_struct(&tree, Arc::default()));
            self.trees.insert(uri.clone(), tree);
        }
        // Members read their workspace dependencies from
        // the root, so a root change must analyse every
        // open file.
        let is_root = self.workspaces.values().any(|root| *root == uri);
        self.analyze((!is_root).then_some(uri)).await;
        Ok(())
    }

    /// Applies one client edit to the text. A `range` of
    /// `None` replaces the whole document. The caller must
    /// call [`Db::reload`] to parse the result.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::NotOpen`] for a ranged edit of a
    /// document that is not open, and [`DbError::Position`]
    /// for a range past its end.
    pub fn update(
        &mut self,
        uri: &Uri,
        range: Option<Range>,
        content: &str,
    ) -> Result<(), DbError>
    {
        let Some(range) = range
        else
        {
            self.files.insert(uri.clone(), Rope::from_str(content));
            return Ok(());
        };
        let file = self
            .files
            .get_mut(uri)
            .context(NotOpenSnafu { uri: uri.clone() })?;
        let (start, end) = char_index(file, range.start)
            .zip(char_index(file, range.end))
            .filter(|(start, end)| start <= end && *end <= file.len_chars())
            .context(PositionSnafu { uri: uri.clone() })?;
        file.remove(start..end);
        file.insert(start, content);
        Ok(())
    }

    /// Loads the manifest at `uri` and its `Cargo.lock`,
    /// then analyses it.
    ///
    /// When `uri` is a workspace member, the workspace root
    /// manifest is loaded first so that `workspace = true`
    /// dependencies resolve. A workspace root that cannot
    /// be found is logged, and the member loads on its own.
    ///
    /// # Errors
    ///
    /// Returns [`DbError`] when a manifest or its lock file
    /// cannot be read.
    pub async fn try_init(
        &mut self,
        uri: &Uri,
    ) -> Result<(), DbError>
    {
        match self.find_workspace_root(uri)
        {
            Ok(Some(root)) =>
            {
                self.workspaces.insert(uri.clone(), root.clone());
                if !self.tomls.contains_key(&root)
                {
                    self.load(&root).await?;
                }
            }
            Ok(None) =>
            {}
            Err(e) =>
            {
                self.client
                    .log_message(MessageType::WARNING, snafu::Report::from_error(e))
                    .await;
            }
        }
        self.load(uri).await
    }

    async fn load(
        &mut self,
        uri: &Uri,
    ) -> Result<(), DbError>
    {
        let path = file_path(uri)?;
        if !self.files.contains_key(uri)
        {
            let text = tokio::fs::read_to_string(&path)
                .await
                .context(ReadSnafu { path: path.clone() })?;
            self.update(uri, None, &text)?;
        }
        if let Some(lock) = lock_uri(&path).filter(|v| !self.locks.contains_key(v))
        {
            self.update_lock(lock).await?;
        }
        self.reload(uri.clone()).await
    }
}

/// Returns the local path of `uri`.
fn file_path(uri: &Uri) -> Result<PathBuf, DbError>
{
    uri.to_file_path()
        .ok()
        .context(NotAFileSnafu { uri: uri.clone() })
}

/// Returns the address of the `Cargo.lock` beside
/// `manifest`.
fn lock_uri(manifest: &Path) -> Option<Uri>
{
    Url::from_file_path(manifest.with_file_name("Cargo.lock")).ok()
}

/// Returns the character index of `position`, or `None`
/// past the last line.
fn char_index(
    file: &Rope,
    position: Position,
) -> Option<usize>
{
    let line = file.try_line_to_char(position.line as usize).ok()?;
    Some(line + position.character as usize)
}
