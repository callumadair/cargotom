use std::{
    fs,
    sync::{Arc, Mutex},
};

use info_provider::InfoProvider;
use parser::Db;
use tower_lsp::{
    Client, LanguageServer, LspService,
    jsonrpc::Result,
    lsp_types::{InitializeParams, InitializeResult},
};
use url::Url;

struct TestContext;

#[tower_lsp::async_trait]
impl LanguageServer for TestContext {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult::default())
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

fn make_client() -> Client {
    let holder = Arc::new(Mutex::new(None));
    let holder2 = holder.clone();
    let (_service, _socket) = LspService::new(move |client| {
        *holder2.lock().unwrap() = Some(client);
        TestContext
    });
    let client = holder.lock().unwrap().take().unwrap();
    client
}

// A member matched by a glob, opened before its root, must still resolve the
// root so that `workspace = true` dependencies have a source.
#[tokio::test]
async fn member_opened_first_finds_glob_root() {
    let dir = tempfile::tempdir().unwrap();
    let member = dir.path().join("crates").join("a");
    fs::create_dir_all(member.join("src")).unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.dependencies]\nserde = \"1\"\n",
    )
    .unwrap();
    fs::write(
        member.join("Cargo.toml"),
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde.workspace = true\n",
    )
    .unwrap();
    fs::write(member.join("src").join("lib.rs"), "").unwrap();

    let member_uri = Url::from_file_path(member.join("Cargo.toml")).unwrap();
    let root_uri = Url::from_file_path(dir.path().join("Cargo.toml")).unwrap();
    let info = Arc::new(InfoProvider::new(50, false, dir.path().to_path_buf()).await);
    let db = Db::new(make_client(), info);
    db.write("test").await.sel = Some(db.clone());

    db.write("test").await.try_init(&member_uri).await;

    let lock = db.read("test").await;
    assert_eq!(lock.get_workspace(&member_uri), Some(&root_uri));
    let root = lock.get_toml(&root_uri).unwrap();
    assert!(root.workspace);
    assert_eq!(root.dependencies[0].data.name(), "serde");
}
