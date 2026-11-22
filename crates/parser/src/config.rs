use info_provider::api::ViewMode;
use serde::{
    Deserialize,
    Serialize,
};

/// Client settings. A field that the client leaves out
/// takes its value from [`Config::default`].
#[derive(Deserialize, Serialize)]
#[serde(default)]
pub struct Config
{
    /// Most crates that one search completion lists.
    /// Default 25.
    pub per_page:                u32,
    /// Which features a crate hover lists. Default
    /// [`ViewMode::UnusedOpt`].
    pub feature_display_mode:    ViewMode,
    /// Hides the hint on the first line of every manifest.
    /// Default `false`.
    pub hide_docs_info_message:  bool,
    /// Sorts keys and arrays when the manifest is
    /// formatted. Default `false`.
    pub sort_format:             bool,
    /// Skips pre-release versions when picking the newest
    /// version. Default `true`.
    pub stable_version:          bool,
    /// Warns when a newer version than the requirement
    /// exists. Default `true`.
    pub outdated_crate_warnings: bool,
}

impl Default for Config
{
    fn default() -> Self
    {
        Self {
            per_page:                25,
            feature_display_mode:    ViewMode::UnusedOpt,
            hide_docs_info_message:  false,
            sort_format:             false,
            stable_version:          true,
            outdated_crate_warnings: true,
        }
    }
}
