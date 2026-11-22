use std::fmt::{self, Display};

use cargo_util_schemas::{core::SourceKind, lockfile::TomlLockfileDependency};

/// Number of commit hash characters shown for a git dependency.
const SHORT_REV_LEN: usize = 7;

/// A locked package shown as an inlay hint.
///
/// `Display` prints the locked version, followed by `#` and the short commit
/// hash for a git source.
#[derive(Debug, Clone, Copy)]
pub struct LockedPackage<'a>(pub &'a TomlLockfileDependency);

impl Display for LockedPackage<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let package = self.0;
        let rev = package
            .source
            .as_ref()
            .filter(|source| matches!(source.kind(), SourceKind::Git(_)))
            .and_then(|source| source.url().fragment());
        match rev {
            Some(rev) => write!(
                f,
                "{} #{}",
                package.version,
                rev.get(..SHORT_REV_LEN).unwrap_or(rev)
            ),
            None => package.version.fmt(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use cargo_util_schemas::lockfile::TomlLockfile;

    use super::*;

    const LOCK: &str = r#"
version = 4

[[package]]
name = "serde"
version = "1.0.228"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "taplo"
version = "0.13.2"
source = "git+https://github.com/frederik-uni/taplo#0123456789abcdef"

[[package]]
name = "short"
version = "0.1.0"
source = "git+https://example.com/short?branch=main#abc"
"#;

    #[test]
    fn display_adds_short_rev_for_git_only() {
        let lock = toml::from_str::<TomlLockfile>(LOCK).unwrap();
        let labels = lock
            .package
            .unwrap()
            .iter()
            .map(|package| LockedPackage(package).to_string())
            .collect::<Vec<_>>();
        assert_eq!(labels, ["1.0.228", "0.13.2 #0123456", "0.1.0 #abc"]);
    }
}
