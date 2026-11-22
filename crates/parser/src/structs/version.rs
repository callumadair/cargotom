use cargo_util_schemas::core::PartialVersion;
use semver::{
    Comparator,
    Op,
    Prerelease,
    Version,
};

/// Returns `true` when `version` is above every version
/// that `requirement` matches.
///
/// Pre-releases count. For `1.2` this holds for `1.3.0` and
/// `2.0.0-rc.1`, but not for `1.2.9`.
pub fn is_newer(
    requirement: &PartialVersion,
    version: &Version,
) -> bool
{
    let above = Comparator {
        op:    Op::Greater,
        major: requirement.major,
        minor: requirement.minor,
        patch: requirement.patch,
        pre:   requirement.pre.clone().unwrap_or_default(),
    };
    // semver never lets a pre-release satisfy a comparator
    // on another major.minor.patch, so compare the release
    // that the pre-release leads to.
    let release = Version {
        pre: Prerelease::EMPTY,
        ..version.clone()
    };
    above.matches(&release)
}

#[cfg(test)]
mod tests
{
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::same_minor("1.2", "1.2.9", false)]
    #[case::next_minor("1.2", "1.3.0", true)]
    #[case::next_major_pre_release("1.2", "2.0.0-rc.1", true)]
    #[case::major_only_same_major("1", "1.99.0", false)]
    #[case::major_only_next_major("1", "2.0.0", true)]
    #[case::exact_equal("1.2.3", "1.2.3", false)]
    #[case::exact_next_patch("1.2.3", "1.2.4", true)]
    #[case::older("1.2", "1.1.9", false)]
    fn is_newer_holds_only_past_the_prefix(
        #[case] requirement: &str,
        #[case] version: &str,
        #[case] expected: bool,
    ) -> Result<(), Box<dyn std::error::Error>>
    {
        let requirement = requirement.parse::<PartialVersion>()?;
        let version = version.parse()?;
        assert_eq!(is_newer(&requirement, &version), expected);
        Ok(())
    }
}
