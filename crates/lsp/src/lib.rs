//! The cargotom language server for `Cargo.toml` files.
//!
//! [`main`] serves the Language Server Protocol over stdin
//! and stdout.

mod actions;
mod completion;
mod hover;
mod lsp;
mod utils;
pub use lsp::main;
