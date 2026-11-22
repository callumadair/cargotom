//! The cargotom language server for `Cargo.toml` files.
//!
//! [`main`] serves the Language Server Protocol over stdin and stdout.

pub mod actions;
mod hover;
mod lsp;
mod utils;
pub use lsp::main;
