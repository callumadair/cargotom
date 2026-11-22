// The server ignores its arguments. Editor extensions still pass `--storage`,
// which named the offline search data directory.
#[tokio::main]
async fn main() {
    lsp::main().await;
}
