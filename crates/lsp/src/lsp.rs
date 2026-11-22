use std::{
    error::Error,
    sync::Arc,
};

use info_provider::InfoProvider;
use parser::{
    Db,
    Indent,
    config::Config,
    static_structure,
};
use snafu::ResultExt;
use tokio::sync::RwLock;
use tower_lsp::{
    Client,
    LanguageServer,
    LspService,
    Server,
    async_trait,
    jsonrpc::{
        self,
        Result,
    },
    lsp_types::{
        CodeActionOrCommand,
        CodeActionParams,
        CodeActionProviderCapability,
        CodeActionResponse,
        CompletionOptions,
        CompletionParams,
        CompletionResponse,
        DidChangeTextDocumentParams,
        DidChangeWorkspaceFoldersParams,
        DidOpenTextDocumentParams,
        DocumentFormattingParams,
        ExecuteCommandOptions,
        ExecuteCommandParams,
        Hover,
        HoverParams,
        HoverProviderCapability,
        InitializeParams,
        InitializeResult,
        InlayHint,
        InlayHintKind,
        InlayHintLabel,
        InlayHintParams,
        MessageType,
        OneOf,
        ServerCapabilities,
        ServerInfo,
        SignatureHelpOptions,
        TextDocumentSyncCapability,
        TextDocumentSyncKind,
        TextDocumentSyncOptions,
        TextEdit,
        Url,
        WorkspaceFoldersServerCapabilities,
        WorkspaceServerCapabilities,
    },
};

use crate::{
    actions::{
        DepActions,
        dep_actions,
        manifest_dir,
    },
    utils::{
        ContextSnafu,
        command_action,
        first_line_actions,
        has_file_name,
        report,
    },
};

/// The commands that `execute_command` runs.
const COMMANDS: [&str; 3] = ["open_url", "cargo-update", "open-src"];

/// The language server. Every handler shares one [`Db`].
pub struct Context
{
    pub client:    Client,
    pub(crate) db: Arc<RwLock<Db>>,
    pub info:      Arc<InfoProvider>,
}

/// Reads the client settings from the initialisation
/// options. A client that sends none gets
/// [`Config::default`].
fn load_config(params: &InitializeParams) -> serde_json::Result<Config>
{
    params
        .initialization_options
        .clone()
        .map(serde_json::from_value)
        .transpose()
        .map(Option::unwrap_or_default)
}

/// Returns the address of the `Cargo.toml` in the folder at
/// `root`, or `None` when `root` is not a local folder.
fn manifest_uri(root: &Url) -> Option<Url>
{
    Url::from_file_path(root.to_file_path().ok()?.join("Cargo.toml")).ok()
}

impl Context
{
    async fn log_error(
        &self,
        error: &(dyn Error + Send + Sync + 'static),
    )
    {
        self.client
            .log_message(MessageType::ERROR, report(error))
            .await;
    }

    /// Loads the manifest in the workspace folder `root`.
    /// The db stays locked only for this one folder.
    async fn init_root(
        &self,
        root: &Url,
    )
    {
        let Some(manifest) = manifest_uri(root)
        else
        {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!("{root} is not a local folder"),
                )
                .await;
            return;
        };
        let result = self.db.write().await.try_init(&manifest).await;
        if let Err(e) = result
        {
            self.log_error(&e).await;
        }
    }

    /// Applies an open or a change of `uri`. A `Cargo.lock`
    /// is read from disk, and a `Cargo.toml` takes
    /// `changes` as `(range, text)` pairs.
    async fn apply(
        &self,
        uri: Url,
        changes: Vec<(Option<tower_lsp::lsp_types::Range>, String)>,
    )
    {
        let result = if has_file_name(&uri, "Cargo.lock")
        {
            self.db.write().await.update_lock(uri).await
        }
        else if has_file_name(&uri, "Cargo.toml")
        {
            let mut db = self.db.write().await;
            match changes
                .iter()
                .try_for_each(|(range, text)| db.update(&uri, *range, text))
            {
                Ok(()) => db.reload(uri).await,
                Err(e) => Err(e),
            }
        }
        else
        {
            return;
        };
        if let Err(e) = result
        {
            self.log_error(&e).await;
        }
    }

    /// Returns the actions for the dependency under
    /// `range`, built under the db lock with one cargo
    /// config.
    async fn dep_actions(
        &self,
        uri: &Url,
        range: tower_lsp::lsp_types::Range,
    ) -> Option<DepActions>
    {
        let result = {
            let db = self.db.read().await;
            self.info
                .context()
                .context(ContextSnafu)
                .and_then(|gctx| Ok(dep_actions(&db, &gctx, uri, range)?))
        };
        match result
        {
            Ok(actions) => actions,
            Err(e) =>
            {
                self.log_error(&e).await;
                None
            }
        }
    }

    /// Runs `cargo update` in the workspace of the manifest
    /// `uri`, and shows a failure to the user.
    async fn cargo_update(
        &self,
        uri: Option<Url>,
    )
    {
        let manifest = match uri
        {
            Some(uri) =>
            {
                self.db
                    .read()
                    .await
                    .get_workspace(&uri)
                    .cloned()
                    .or(Some(uri))
            }
            None => None,
        };
        let Some(dir) = manifest.as_ref().and_then(manifest_dir)
        else
        {
            self.client
                .show_message(
                    MessageType::ERROR,
                    "cargo update needs the address of a manifest",
                )
                .await;
            return;
        };
        let message = match tokio::process::Command::new("cargo")
            .arg("update")
            .current_dir(&dir)
            .output()
            .await
        {
            Ok(output) if output.status.success() => return,
            Ok(output) =>
            {
                format!(
                    "cargo update failed in {}: {}",
                    dir.display(),
                    String::from_utf8_lossy(&output.stderr)
                )
            }
            Err(e) => format!("cannot run cargo update in {}: {e}", dir.display()),
        };
        self.client.show_message(MessageType::ERROR, message).await;
    }

    /// Returns the source repository of the crate, or its
    /// docs.rs source view when the registry lists no
    /// repository.
    async fn source_url(
        &self,
        name: &str,
        version: Option<&str>,
    ) -> Option<String>
    {
        match self.info.get_crate_repository(name).await
        {
            Ok(Some(url)) => return Some(url),
            Ok(None) =>
            {}
            Err(e) => self.log_error(e.as_ref()).await,
        }
        version.map(|version| format!("https://docs.rs/crate/{name}/{version}/source/"))
    }

    async fn open_url(
        &self,
        url: &str,
    ) -> Result<Option<serde_json::Value>>
    {
        if let Err(e) = webbrowser::open(url)
        {
            self.client
                .show_message(MessageType::WARNING, format!("failed to open browser {e}"))
                .await;
            return Err(jsonrpc::Error::invalid_params("failed to open browser"));
        }
        Ok(None)
    }
}

#[async_trait]
impl LanguageServer for Context
{
    async fn did_change_workspace_folders(
        &self,
        params: DidChangeWorkspaceFoldersParams,
    )
    {
        {
            let mut db = self.db.write().await;
            for removed in &params.event.removed
            {
                db.remove_workspace(&removed.uri);
            }
        }
        for added in &params.event.added
        {
            self.init_root(&added.uri).await;
        }
    }

    async fn initialize(
        &self,
        params: InitializeParams,
    ) -> Result<InitializeResult>
    {
        let config = match load_config(&params)
        {
            Ok(config) => config,
            Err(e) =>
            {
                self.log_error(&e).await;
                Config::default()
            }
        };
        if let Err(e) = static_structure::schema()
        {
            self.log_error(e).await;
        }
        self.info.set_per_page(config.per_page).await;
        self.db.write().await.config = config;
        let roots = params
            .workspace_folders
            .unwrap_or_default()
            .into_iter()
            .map(|v| v.uri)
            .chain(params.root_uri);
        for root in roots
        {
            self.init_root(&root).await;
        }
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        ..Default::default()
                    },
                )),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(["\"", ".", ":", "-"].map(String::from).to_vec()),
                    ..Default::default()
                }),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["\"".to_owned()]),
                    ..Default::default()
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                document_formatting_provider: Some(OneOf::Left(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: COMMANDS.map(String::from).to_vec(),
                    ..Default::default()
                }),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported:            Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    file_operations:   None,
                }),
                inlay_hint_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            server_info:  Some(ServerInfo {
                name:    "CargoTom".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
        })
    }

    async fn shutdown(&self) -> Result<()> { Ok(()) }

    async fn did_change(
        &self,
        params: DidChangeTextDocumentParams,
    )
    {
        let changes = params
            .content_changes
            .into_iter()
            .map(|v| (v.range, v.text))
            .collect();
        self.apply(params.text_document.uri, changes).await;
    }

    async fn did_open(
        &self,
        params: DidOpenTextDocumentParams,
    )
    {
        let document = params.text_document;
        self.apply(document.uri, vec![(None, document.text)]).await;
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<CodeActionResponse>>
    {
        let uri = params.text_document.uri;
        if !has_file_name(&uri, "Cargo.toml")
        {
            return Ok(None);
        }
        let mut actions = vec![];
        let dep = self
            .dep_actions(&uri, params.range)
            .await
            .unwrap_or_default();
        if let Some(lookup) = dep.lookup
            && let Some(action) = self.lookup_action(lookup).await
        {
            actions.push(action);
        }
        let mut response: Vec<CodeActionOrCommand> = actions
            .into_iter()
            .chain(dep.edits)
            .map(CodeActionOrCommand::CodeAction)
            .collect();
        if params.range.start.line == 0 || params.range.end.line == 0
        {
            response.splice(0..0, first_line_actions());
        }
        let update = command_action("Update All", "cargo-update", vec![uri.to_string().into()]);
        response.extend(
            std::iter::once(update)
                .chain(dep.links)
                .map(CodeActionOrCommand::CodeAction),
        );
        Ok(Some(response))
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>>
    {
        let arg = |index: usize| params.arguments.get(index).and_then(|v| v.as_str());
        match params.command.as_str()
        {
            "cargo-update" =>
            {
                self.cargo_update(arg(0).and_then(|v| Url::parse(v).ok()))
                    .await;
                Ok(None)
            }
            "open-src" =>
            {
                let name =
                    arg(0).ok_or_else(|| jsonrpc::Error::invalid_params("crate name missing"))?;
                let url = self
                    .source_url(name, arg(1))
                    .await
                    .ok_or_else(|| jsonrpc::Error::invalid_params("crate version missing"))?;
                self.open_url(&url).await
            }
            "open_url" =>
            {
                let url =
                    arg(0).ok_or_else(|| jsonrpc::Error::invalid_params("URL argument missing"))?;
                self.open_url(url).await
            }
            _ => Err(jsonrpc::Error::method_not_found()),
        }
    }

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>>
    {
        let options = params.options;
        let indent = if options.insert_spaces
        {
            Indent::Spaces(options.tab_size)
        }
        else
        {
            Indent::Tab
        };
        let trailing_new_line = options.insert_final_newline.unwrap_or(true);
        let db = self.db.read().await;
        Ok(db
            .format(&params.text_document.uri, trailing_new_line, indent)
            .map(|v| vec![v]))
    }

    async fn inlay_hint(
        &self,
        params: InlayHintParams,
    ) -> Result<Option<Vec<InlayHint>>>
    {
        let uri = params.text_document.uri;
        if !has_file_name(&uri, "Cargo.toml")
        {
            return Ok(None);
        }
        let db = self.db.read().await;
        let hints = db
            .hints(&uri)
            .unwrap_or_default()
            .into_iter()
            .map(|(position, package)| {
                InlayHint {
                    position,
                    label: InlayHintLabel::String(package.to_string()),
                    kind: Some(InlayHintKind::TYPE),
                    text_edits: None,
                    tooltip: None,
                    padding_left: Some(true),
                    padding_right: Some(true),
                    data: None,
                }
            })
            .collect();
        Ok(Some(hints))
    }

    async fn hover(
        &self,
        params: HoverParams,
    ) -> Result<Option<Hover>>
    {
        let position = params.text_document_position_params;
        let uri = position.text_document.uri;
        if !has_file_name(&uri, "Cargo.toml")
        {
            return Ok(None);
        }
        Ok(Context::hover(self, &uri, position.position).await)
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>>
    {
        let position = params.text_document_position;
        let uri = position.text_document.uri;
        if !has_file_name(&uri, "Cargo.toml")
        {
            return Ok(None);
        }
        Ok(self.complete(&uri, position.position).await)
    }
}

/// Serves the language server over stdin and stdout until
/// the client exits.
pub async fn main()
{
    let info = Arc::new(InfoProvider::new(Config::default().per_page));
    let (service, socket) = LspService::build(|client| {
        Context {
            client: client.clone(),
            db: Db::new(client, info.clone()),
            info,
        }
    })
    .finish();
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
        .await;
}
