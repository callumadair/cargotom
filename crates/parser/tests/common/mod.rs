//! Shared fixtures for the parser integration tests.

use std::{
    path::Path,
    sync::Arc,
};

use cargo::GlobalContext;
use cargo_util_terminal::Shell;
use info_provider::InfoProvider;
use parser::{
    Db,
    config::Config,
};
use tokio::sync::RwLock;
use tower_lsp::{
    Client,
    LanguageServer,
    LspService,
    jsonrpc::Result,
    lsp_types::{
        InitializeParams,
        InitializeResult,
    },
};

/// A server that only hands out its client.
struct TestContext(Client);

#[tower_lsp::async_trait]
impl LanguageServer for TestContext
{
    async fn initialize(
        &self,
        _: InitializeParams,
    ) -> Result<InitializeResult>
    {
        Ok(InitializeResult::default())
    }

    async fn shutdown(&self) -> Result<()> { Ok(()) }
}

/// Returns an empty `Db` whose cargo lookups stay offline
/// and read no config outside `root`.
pub fn offline_db(root: &Path) -> Arc<RwLock<Db>>
{
    let (service, _socket) = LspService::new(TestContext);
    let client = service.inner().0.clone();
    let root = root.to_path_buf();
    let info = InfoProvider::with_context(Config::default().per_page, move || {
        let mut gctx = GlobalContext::new(Shell::new(), root.clone(), root.join("cargo-home"));
        gctx.set_env(
            [("CARGO_NET_OFFLINE".to_owned(), "true".to_owned())]
                .into_iter()
                .collect(),
        );
        Ok(gctx)
    });
    Db::new(client, Arc::new(info))
}
