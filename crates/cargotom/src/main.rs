//! The `cargotom` language server binary. It serves LSP over stdin and
//! stdout.
//!
//! The binary ignores its arguments, so an editor extension can pass flags
//! such as `--storage` without effect.

#[tokio::main]
async fn main() {
    lsp::main().await;
}
