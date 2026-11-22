//! A syntax tree of a manifest that keeps the byte range of
//! every key and value, for lookups by cursor position.

use taplo::{
    dom::{
        Node,
        node::{
            DomNode,
            Table,
            TableKind,
        },
    },
    rowan::TextRange,
};

use crate::toml::Positioned;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tree
{
    pub nodes: Vec<TreeValue>,
    pub kind:  TableKind,
    pub pos:   ByteSpan,
}

/// One step of a [`PathValue`] path. `Display` writes the
/// key or the value.
#[derive(Debug, derive_more::Display)]
pub enum Type
{
    /// A table key.
    TreeKey(String),
    /// A string value.
    String(String),
    /// A boolean value.
    Bool(bool),
    /// A value of any other kind, or one that does not
    /// parse.
    #[display("unknown")]
    Unknown,
}

/// One key or value on the path to a cursor, with its byte
/// range.
#[derive(Debug)]
pub struct PathValue
{
    /// The key or value.
    pub ty:    Type,
    /// The byte range of the key or value.
    pub range: ByteSpan,
}

impl PathValue
{
    /// Returns `true` when this step is a value and not a
    /// key. `last` is the step before it. A step that
    /// starts where `last` starts is a key.
    pub fn is_value(
        &self,
        last: Option<&Self>,
    ) -> bool
    {
        if last.is_some_and(|last| self.range.start == last.range.start)
        {
            return false;
        }
        matches!(self.ty, Type::String(_) | Type::Bool(_) | Type::Unknown)
    }
}

impl Tree
{
    /// Returns the keys and the value under the byte offset
    /// `cursor`, outermost first. Returns an empty path
    /// when no key or value holds `cursor`.
    pub fn path(
        &self,
        cursor: usize,
    ) -> Vec<PathValue>
    {
        let on_key_or_value = self.nodes.iter().find_map(|node| {
            if let Some(range) = node.key.ranges.iter().find(|v| v.touches(cursor))
            {
                return Some(vec![PathValue {
                    ty:    Type::TreeKey(node.key.value.clone()),
                    range: *range,
                }]);
            }
            let items = node.value.path(cursor);
            let start = items.first()?.range.start;
            let mut out = vec![PathValue {
                ty:    Type::TreeKey(node.key.value.clone()),
                range: node.key.closest_range(start),
            }];
            out.extend(items);
            Some(out)
        });
        on_key_or_value
            .or_else(|| {
                self.nodes
                    .iter()
                    .find(|v| v.pos.touches(cursor))
                    .map(|node| {
                        vec![PathValue {
                            ty:    Type::TreeKey(node.key.value.clone()),
                            range: node.key.closest_range(node.pos.end),
                        }]
                    })
            })
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeValue
{
    pub key:   Key,
    pub value: Value,
    pub pos:   ByteSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Key
{
    pub(crate) ranges: Vec<ByteSpan>,
    pub(crate) value:  String,
}

impl Key
{
    /// Returns the occurrence of the key that ends last
    /// before `value_start`, or the first occurrence when
    /// none does. A key without syntax gives an empty span
    /// at 0.
    pub fn closest_range(
        &self,
        value_start: u32,
    ) -> ByteSpan
    {
        self.ranges
            .iter()
            .filter(|v| v.end <= value_start)
            .max_by_key(|v| v.end)
            .or_else(|| self.ranges.first())
            .copied()
            .unwrap_or_default()
    }

    pub fn to_positioned(
        &self,
        value_start: u32,
    ) -> Positioned<String>
    {
        let range = self.closest_range(value_start);
        Positioned::new(range.start, range.end, self.value.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value
{
    Tree
    {
        value: Tree,
    },
    NoContent,
    Array
    {
        value: Vec<Value>,
        range: ByteSpan,
    },
    String
    {
        value: String,
        range: ByteSpan,
    },
    Bool
    {
        value: bool,
        range: ByteSpan,
    },
    Unknown(ByteSpan),
}

impl Value
{
    pub fn path(
        &self,
        position: usize,
    ) -> Vec<PathValue>
    {
        let leaf = |ty: Type, range: &ByteSpan| {
            match range.touches(position)
            {
                true => vec![PathValue { ty, range: *range }],
                false => vec![],
            }
        };
        match self
        {
            Value::Tree { value } => value.path(position),
            Value::NoContent => vec![],
            Value::Array { value, .. } =>
            {
                value
                    .iter()
                    .map(|v| v.path(position))
                    .find(|items| !items.is_empty())
                    .unwrap_or_default()
            }
            Value::String { value, range } => leaf(Type::String(value.to_owned()), range),
            Value::Bool { value, range } => leaf(Type::Bool(*value), range),
            Value::Unknown(range) => leaf(Type::Unknown, range),
        }
    }

    pub fn as_str(&self) -> Option<Positioned<String>>
    {
        match self
        {
            Value::String { value, range } => Some(str_to_positioned(value, range)),
            _ => None,
        }
    }

    pub fn as_tree(&self) -> Option<&Tree>
    {
        match self
        {
            Value::Tree { value } => Some(value),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<Positioned<bool>>
    {
        match self
        {
            Value::Bool { value, range } => Some(Positioned::new(range.start, range.end, *value)),
            _ => None,
        }
    }

    pub fn range(&self) -> Option<ByteSpan>
    {
        match self
        {
            Value::Tree { value } => Some(value.pos),
            Value::NoContent => None,
            Value::Array { range, .. }
            | Value::String { range, .. }
            | Value::Bool { range, .. }
            | Value::Unknown(range) => Some(*range),
        }
    }
}

/// A byte range in a document.
///
/// [`ByteSpan::touches`] counts `end` as inside, so that a
/// cursor just after the last byte of a key or value still
/// selects it.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
pub struct ByteSpan
{
    /// The offset of the first byte.
    pub start: u32,
    /// The offset after the last byte.
    pub end:   u32,
}

impl<T> From<&Positioned<T>> for ByteSpan
{
    fn from(value: &Positioned<T>) -> Self { Self::new(value.start, value.end) }
}

impl From<TextRange> for ByteSpan
{
    fn from(value: TextRange) -> Self { Self::new(value.start().into(), value.end().into()) }
}

impl ByteSpan
{
    /// Returns the span from `start` to `end`.
    pub fn new(
        start: u32,
        end: u32,
    ) -> Self
    {
        Self { start, end }
    }

    /// Returns `true` when `offset` is in the span or
    /// equals `end`.
    pub fn touches(
        &self,
        offset: usize,
    ) -> bool
    {
        u32::try_from(offset).is_ok_and(|offset| self.start <= offset && offset <= self.end)
    }

    /// Returns the smallest span that covers both spans.
    pub fn join(
        &self,
        other: &ByteSpan,
    ) -> Self
    {
        Self::new(self.start.min(other.start), self.end.max(other.end))
    }
}

/// Returns the byte span of a DOM node, or an empty span at
/// 0 for a node without syntax.
fn span(node: &impl DomNode) -> ByteSpan
{
    node.syntax()
        .map(|s| s.text_range().into())
        .unwrap_or_default()
}

impl From<&Table> for Tree
{
    fn from(table: &Table) -> Self
    {
        let nodes: Vec<TreeValue> = table
            .entries()
            .get()
            .iter()
            .map(|(key, value)| {
                let key = Key {
                    ranges: key.text_ranges().map(ByteSpan::from).collect(),
                    value:  key.value().to_string(),
                };
                let node_range = span(value);
                let value = Value::from(value);
                let closest_range = key.closest_range(node_range.end);
                let range = value.range();
                TreeValue {
                    pos: range
                        .map_or(closest_range, |v| v.join(&closest_range))
                        .join(&node_range),
                    value: match range == Some(closest_range)
                    {
                        true => Value::NoContent,
                        false => value,
                    },
                    key,
                }
            })
            .collect();
        let pos = nodes
            .iter()
            .map(|v| v.pos)
            .reduce(|a, b| a.join(&b))
            .unwrap_or_else(|| span(table));
        Self {
            nodes,
            kind: table.kind(),
            pos,
        }
    }
}

impl From<&Node> for Value
{
    fn from(node: &Node) -> Self
    {
        let range = span(node);
        match node
        {
            Node::Table(table) =>
            {
                Value::Tree {
                    value: Tree::from(table),
                }
            }
            Node::Array(arr) =>
            {
                Value::Array {
                    value: arr.items().get().iter().map(Self::from).collect(),
                    range,
                }
            }
            Node::Bool(b) =>
            {
                Value::Bool {
                    value: b.value(),
                    range,
                }
            }
            Node::Str(s) =>
            {
                Value::String {
                    value: s.value().to_string(),
                    range,
                }
            }
            // Text that does not parse yet, kept so that
            // completion can use it.
            Node::Invalid(invalid) =>
            {
                Value::String {
                    value: invalid
                        .syntax()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                    range,
                }
            }
            Node::Integer(_) | Node::Float(_) | Node::Date(_) => Value::Unknown(range),
        }
    }
}

/// Returns `str` positioned at `range`.
pub fn str_to_positioned(
    str: &str,
    range: &ByteSpan,
) -> Positioned<String>
{
    Positioned::new(range.start, range.end, str.to_string())
}

#[cfg(test)]
mod tests
{
    use rstest::rstest;

    use super::*;

    /// Returns the path at the first `|` in `source`, with
    /// the marker removed.
    fn path_at(source: &str) -> Result<Vec<String>, &'static str>
    {
        let cursor = source.find('|').ok_or("the source has no cursor marker")?;
        let text = source.replacen('|', "", 1);
        let dom = taplo::parser::parse(&text).into_dom();
        let tree = Tree::from(dom.as_table().ok_or("the document root is not a table")?);
        Ok(tree.path(cursor).iter().map(|v| v.ty.to_string()).collect())
    }

    #[rstest]
    #[case::string_after_integer("a = 1\nb = \"x|y\"", &["b", "xy"])]
    #[case::integer("a = 1|2\nb = \"x\"", &["a", "unknown"])]
    #[case::key("a = 1\nb| = \"x\"", &["b"])]
    #[case::outside("a = 1\n\n|", &[])]
    fn path_finds_the_value_under_the_cursor(
        #[case] source: &str,
        #[case] expected: &[&str],
    ) -> Result<(), &'static str>
    {
        assert_eq!(path_at(source)?, expected);
        Ok(())
    }
}
