//! Readers that turn syntax tree nodes into manifest data.

/// Dependency tables.
pub mod deps;
/// The `[features]` table.
pub mod feat;
/// `Cargo.lock` entries.
pub mod lock;
/// Version requirement checks.
pub mod version;
