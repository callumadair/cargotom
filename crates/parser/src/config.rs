use info_provider::api::ViewMode;
use serde::{Deserialize, Serialize};

/// Client settings. A field that the client leaves out takes its value from
/// [`Config::default`].
#[derive(Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub per_page: u32,
    pub feature_display_mode: ViewMode,
    pub hide_docs_info_message: bool,
    pub sort_format: bool,
    pub stable_version: bool,
    pub outdated_crate_warnings: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            per_page: 25,
            feature_display_mode: ViewMode::UnusedOpt,
            hide_docs_info_message: false,
            sort_format: false,
            stable_version: true,
            outdated_crate_warnings: true,
        }
    }
}
