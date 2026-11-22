use taplo::formatter::Options;

use crate::{Db, Indent, Uri};

impl Db {
    /// Formats the manifest at `uri` with taplo.
    ///
    /// Returns one edit that replaces the whole document, as
    /// `((start line, start char), (end line, end char))` and the new text.
    /// Returns `None` when the document is not open.
    pub fn format(
        &self,
        uri: &Uri,
        trailing_new_line: bool,
        indent: Indent,
    ) -> Option<Vec<(((usize, usize), (usize, usize)), String)>> {
        let content = self.get_content(uri)?;
        let sort = self.config.sort_format;
        let new = taplo::formatter::format(
            &content,
            Options {
                indent_string: indent.to_string(),
                trailing_newline: trailing_new_line,
                reorder_keys: sort,
                reorder_arrays: sort,
                allowed_blank_lines: 2,
                crlf: content.contains("\r\n"),
                ..Default::default()
            },
        );
        let end = self.get_last_line_and_char(uri).unwrap_or((0, 0));
        Some(vec![(((0, 0), end), new)])
    }
}
