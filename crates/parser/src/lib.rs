mod analyze;
pub mod config;
mod format;
pub mod static_structure;
pub mod structs;
pub mod toml;
pub mod tree;
mod tree_to_struct;

use std::{
    collections::HashMap, fs::read_to_string, panic::catch_unwind, path::PathBuf, sync::Arc,
};

use cargo::GlobalContext;
use config::Config;
use info_provider::InfoProvider;
use ropey::Rope;
use static_structure::{parse_all, Parsed};
use cargo_util_schemas::lockfile::{TomlLockfile, TomlLockfileDependency};
use structs::lock::LockedPackage;
use tokio::sync::RwLock;
use toml::{Dependency, Positioned, Toml};
use tower_lsp::{lsp_types::MessageType, Client};
use tree::{PathValue, RangeExclusive, Tree};
use tree_to_struct::to_struct;
use url::Url;

pub type Uri = url::Url;
pub struct Db {
    pub sel: Option<Arc<RwLock<Db>>>,
    pub client: Client,
    pub static_data: Parsed,
    files: HashMap<Uri, Rope>,
    trees: HashMap<Uri, Tree>,
    tomls: HashMap<Uri, Toml>,
    info: Arc<InfoProvider>,
    /// Workspace root manifest of each member manifest.
    workspaces: HashMap<Uri, Uri>,
    locks: HashMap<Uri, TomlLockfile>,
    pub warnings: Arc<RwLock<HashMap<Uri, Vec<Warning>>>>,
    pub config: Config,
}

/// Returns the root manifest of the workspace that lists `manifest` as a
/// member, read from disk by cargo.
///
/// Returns `None` for a workspace root, for a package outside any workspace,
/// and when cargo cannot read a manifest on the way up.
fn find_workspace_root(manifest: &Uri) -> Option<Uri> {
    let path = manifest.to_file_path().ok()?;
    let gctx = GlobalContext::default().ok()?;
    let root = cargo::workspace::find_workspace_root(&path, &gctx).ok()??;
    Url::from_file_path(root).ok()
}

#[derive(Debug, Clone)]
pub enum Level {
    Warn,
    Error,
}

#[derive(Debug, Clone)]
pub struct Warning {
    level: Level,
    msg: String,
    start: (usize, usize),
    end: (usize, usize),
}

impl Db {
    pub fn new(client: Client, info: Arc<InfoProvider>) -> Arc<RwLock<Db>> {
        Arc::new(RwLock::new(Self {
            static_data: parse_all(),
            config: Config::default(),
            sel: Default::default(),
            client,
            info,
            files: HashMap::new(),
            trees: HashMap::new(),
            tomls: HashMap::new(),
            workspaces: Default::default(),
            locks: HashMap::new(),
            warnings: Default::default(),
        }))
    }
}

/// One level of indentation. `Display` writes the indent string.
#[derive(derive_more::Display)]
pub enum Indent {
    #[display("{}", " ".repeat(*_0 as usize))]
    Spaces(u32),
    #[display("\t")]
    Tab,
}

impl Db {
    pub async fn get_path(&self, uri: &Uri, line: u32, char: u32) -> Option<Vec<PathValue>> {
        let byte = self.get_byte(uri, line as usize, char as usize)?;
        let tree = self.trees.get(uri)?;
        self.client
            .log_message(MessageType::INFO, format!("{:#?}", tree))
            .await;
        let v = tree.path(byte);
        match v.is_empty() {
            true => None,
            false => Some(v),
        }
    }

    pub fn remove_workspace(&mut self, workspace_uri: &Url) {
        self.files
            .retain(|uri, _| !Self::is_within_workspace(uri, workspace_uri));
        self.trees
            .retain(|uri, _| !Self::is_within_workspace(uri, workspace_uri));
        self.tomls
            .retain(|uri, _| !Self::is_within_workspace(uri, workspace_uri));
    }

    fn is_within_workspace(file_uri: &Url, workspace_uri: &Url) -> bool {
        file_uri.as_str().starts_with(workspace_uri.as_str())
    }

    /// Returns the first `Cargo.lock` entry named `name` for the workspace that
    /// holds `uri`.
    pub fn get_locked_package(&self, uri: &Uri, name: &str) -> Option<&TomlLockfileDependency> {
        let mut file = self.workspaces.get(uri).unwrap_or(uri).clone();
        if let Ok(mut v) = file.path_segments_mut() {
            v.pop();
            v.push("Cargo.lock");
        }
        self.locks
            .get(&file)?
            .package
            .as_ref()?
            .iter()
            .find(|package| package.name == name)
    }

    pub async fn hints(&self, uri: &Uri) -> Option<Vec<((usize, usize), LockedPackage<'_>)>> {
        let toml = self.tomls.get(uri)?;
        let data = toml
            .dependencies
            .iter()
            .filter_map(|v| {
                let pos = self.get_offset(uri, v.end as usize)?;
                let package = self.get_locked_package(uri, &v.data.crate_name())?;
                Some((pos, LockedPackage(package)))
            })
            .collect::<Vec<_>>();
        Some(data)
    }
    pub fn get_content(&self, uri: &Uri) -> Option<String> {
        Some(self.files.get(uri)?.to_string())
    }
    pub fn get_line(&self, uri: &Uri, bytes_offset: usize) -> Option<usize> {
        if let Some(v) = self.files.get(uri) {
            let line_index = catch_unwind(|| v.byte_to_line(bytes_offset)).ok()?;
            return Some(line_index);
        }
        None
    }

    pub fn get_workspace(&self, uri: &Uri) -> Option<&Uri> {
        self.workspaces.get(uri)
    }

    pub fn get_toml(&self, uri: &Uri) -> Option<&Toml> {
        self.tomls.get(uri)
    }

    pub fn get_byte(&self, uri: &Uri, line: usize, char: usize) -> Option<usize> {
        if let Some(v) = self.files.get(uri) {
            let line_chars = catch_unwind(|| v.line_to_char(line)).ok()?;
            let byte = catch_unwind(|| v.char_to_byte(line_chars + char)).ok()?;
            return Some(byte);
        }
        None
    }

    pub fn get_last_line_and_char(&self, uri: &Uri) -> Option<(usize, usize)> {
        if let Some(v) = self.files.get(uri) {
            let total_chars = v.len_chars();

            if total_chars == 0 {
                return Some((0, 0));
            }

            let last_line_index = v.len_lines() - 1;

            let last_line_start_char = v.line_to_char(last_line_index);

            let chars_in_last_rows = total_chars - last_line_start_char;

            Some((last_line_index, chars_in_last_rows))
        } else {
            None
        }
    }

    pub fn get_offset(&self, uri: &Uri, byte_offset: usize) -> Option<(usize, usize)> {
        if let Some(v) = self.files.get(uri) {
            let line_index = catch_unwind(|| v.byte_to_line(byte_offset)).ok()?;
            let line_start_char = catch_unwind(|| v.line_to_char(line_index)).ok()?;
            let char_offset = catch_unwind(|| v.byte_to_char(byte_offset)).ok()?;
            let char_offset_in_line = char_offset - line_start_char;
            return Some((line_index, char_offset_in_line));
        }
        None
    }
    pub async fn update_lock(&mut self, uri: Uri) {
        if let Ok(path) = uri.to_file_path() {
            if let Ok(str) = read_to_string(path) {
                if let Ok(lock) = ::toml::from_str::<TomlLockfile>(&str) {
                    self.locks.insert(uri, lock);
                }
            }
        }

        self.analyze(None).await;
    }

    pub fn get_dependency(
        &self,
        uri: &Uri,
        (ls, cs): (usize, usize),
        (le, ce): (usize, usize),
    ) -> Option<&Positioned<Dependency>> {
        let file = self.files.get(uri)?;
        let cs = catch_unwind(|| file.line_to_char(ls) + cs).ok()?;
        let bs = catch_unwind(|| file.char_to_byte(cs)).ok()?;
        let ce = catch_unwind(|| file.line_to_char(le) + ce).ok()?;
        let be = catch_unwind(|| file.char_to_byte(ce)).ok()?;
        let toml = self.tomls.get(uri)?;
        let found = toml
            .dependencies
            .iter()
            .find(|v| v.overlap(RangeExclusive::new(bs as u32, be as u32)))?;
        Some(found)
    }

    pub async fn reload(&mut self, uri: Uri) -> Option<()> {
        if let Some(content) = self.files.get(&uri) {
            self.add_content(uri.clone(), &content.to_string());
            if let Some(tree) = self.trees.get(&uri) {
                self.tomls.insert(uri.clone(), to_struct(tree, Arc::default()));
            }
        }
        // Members read their workspace dependencies from the root, so a root
        // change must analyse every open file.
        let is_root = self.workspaces.values().any(|root| *root == uri);
        self.analyze((!is_root).then_some(uri)).await;
        Some(())
    }

    pub fn update(
        &mut self,
        uri: &Uri,
        range: Option<((usize, usize), (usize, usize))>,
        content: &str,
    ) -> Option<()> {
        if let Some(((sl, sc), (el, ec))) = range {
            let file = self.files.get_mut(uri)?;
            let start = catch_unwind(|| file.line_to_char(sl) + sc).ok()?;
            let end = catch_unwind(|| file.line_to_char(el) + ec).ok()?;
            file.remove(start..end);
            file.insert(start, content);
        } else {
            self.files.insert(uri.clone(), Rope::from_str(content));
        }
        Some(())
    }

    /// Loads the manifest at `uri` and its `Cargo.lock`, then analyses it.
    ///
    /// When `uri` is a workspace member, the workspace root manifest is
    /// loaded first so that `workspace = true` dependencies resolve.
    pub async fn try_init(&mut self, uri: &Uri) -> Option<()> {
        if let Some(root) = find_workspace_root(uri) {
            self.workspaces.insert(uri.clone(), root.clone());
            if !self.tomls.contains_key(&root) {
                self.load(&root).await;
            }
        }
        self.load(uri).await
    }

    async fn load(&mut self, uri: &Uri) -> Option<()> {
        if !self.files.contains_key(uri) {
            self.add_file(uri);
        }

        let file = Url::from_file_path(
            uri.to_file_path()
                .ok()?
                .parent()
                .map(|v| v.join("Cargo.lock"))
                .unwrap_or(PathBuf::from("Cargo.lock")),
        )
        .ok()?;
        if !self.locks.contains_key(&file) {
            self.update_lock(file).await;
        }

        self.reload(uri.clone()).await;

        Some(())
    }

    fn add_file(&mut self, file: &Uri) {
        let path = file.to_string();
        if let Some(path) = path.strip_prefix("file://") {
            if let Ok(data) = read_to_string(path) {
                self.update(file, None, &data);
            }
        }
    }

    fn add_content(&mut self, uri: Uri, content: &str) {
        let dom = taplo::parser::parse(content).into_dom();

        let tree = dom.as_table().map(Tree::from);

        if let Some(tree) = tree {
            self.trees.insert(uri, tree);
        }
    }
}
