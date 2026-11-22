use taplo::formatter::Options;
use tower_lsp::lsp_types::TextEdit;

use crate::{
    Db,
    Indent,
    Uri,
};

/// Blank lines that the formatter keeps between two
/// entries.
const ALLOWED_BLANK_LINES: usize = 2;

impl Db
{
    /// Formats the manifest at `uri` with taplo and returns
    /// one edit that replaces the whole document. Returns
    /// `None` when the document is not open.
    pub fn format(
        &self,
        uri: &Uri,
        trailing_new_line: bool,
        indent: Indent,
    ) -> Option<TextEdit>
    {
        let content = self.get_content(uri)?;
        let sort = self.config.sort_format;
        let new_text = taplo::formatter::format(
            &content,
            Options {
                indent_string: indent.to_string(),
                trailing_newline: trailing_new_line,
                reorder_keys: sort,
                reorder_arrays: sort,
                allowed_blank_lines: ALLOWED_BLANK_LINES,
                crlf: content.contains("\r\n"),
                ..Default::default()
            },
        );
        Some(TextEdit::new(self.full_range(uri)?, new_text))
    }
}
