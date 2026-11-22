use std::error::Error;

use info_provider::InfoError;
use parser::toml::ManifestTextError;
use snafu::Snafu;
use tower_lsp::lsp_types::{
    CodeAction,
    CodeActionKind,
    CodeActionOrCommand,
    Command,
    Url,
};

/// The title and address of each link that the first line
/// of a manifest offers.
const FIRST_LINE_LINKS: [(&str, &str); 3] = [
    (
        "Open LSP docs",
        "https://github.com/frederik-uni/cargotom/blob/main/README.md",
    ),
    (
        "Report LSP issue/Suggest feature",
        "https://github.com/frederik-uni/cargotom/issues",
    ),
    (
        "Open Cargo manifest docs",
        "https://doc.rust-lang.org/cargo/reference/manifest.html",
    ),
];

/// Returns whether the last path segment of `uri` is
/// `name`.
pub fn has_file_name(
    uri: &Url,
    name: &str,
) -> bool
{
    uri.path_segments().and_then(|mut v| v.next_back()) == Some(name)
}

/// Returns a code action that runs the server command
/// `command`.
pub fn command_action(
    title: impl Into<String>,
    command: &str,
    arguments: Vec<serde_json::Value>,
) -> CodeAction
{
    let title = title.into();
    CodeAction {
        title: title.clone(),
        kind: Some(CodeActionKind::EMPTY),
        command: Some(Command {
            title,
            command: command.to_owned(),
            arguments: Some(arguments),
        }),
        ..CodeAction::default()
    }
}

/// Returns the actions that open the project docs and the
/// issue tracker.
pub fn first_line_actions() -> impl Iterator<Item = CodeActionOrCommand>
{
    FIRST_LINE_LINKS.iter().map(|(title, url)| {
        CodeActionOrCommand::CodeAction(command_action(*title, "open_url", vec![(*url).into()]))
    })
}

/// An edit that the server cannot write.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum EditError
{
    /// Cargo cannot set up its config.
    #[snafu(display("cannot set up cargo"))]
    Context
    {
        /// The cargo error.
        source: InfoError,
    },
    /// Cargo cannot write a dependency.
    #[snafu(transparent)]
    Text
    {
        /// The cargo error.
        source: ManifestTextError,
    },
}

/// Formats `error` with its chain of causes.
pub fn report(error: &(dyn Error + 'static)) -> String
{
    snafu::Report::from_error(error).to_string()
}
