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
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::registry("registry+https://github.com/rust-lang/crates.io-index", "1.0.0")]
    #[case::git_long_rev("git+https://example.com/foo#0123456789abcdef", "1.0.0 #0123456")]
    #[case::git_short_rev("git+https://example.com/foo?branch=main#abc", "1.0.0 #abc")]
    fn display_adds_short_rev_for_git_only(#[case] source: &str, #[case] expected: &str) {
        let lock = format!(
            "version = 4\n\n[[package]]\nname = \"foo\"\nversion = \"1.0.0\"\nsource = \"{source}\"\n"
        );
        let lock = toml::from_str::<TomlLockfile>(&lock).unwrap();
        let package = &lock.package.unwrap()[0];
        assert_eq!(LockedPackage(package).to_string(), expected);
    }
}
