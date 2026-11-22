//! The manifest schema, for key and value completion and
//! hover text.
//!
//! The schema ships as `cargo.json` in this crate.
//! [`schema`] returns it as a [`Parsed`] tree. A key path
//! such as `["dependencies", "serde", "features"]` walks
//! that tree.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        LazyLock,
    },
};

use indexmap::IndexMap;
use regex::bytes::Regex;
use serde::Deserialize;
use snafu::{
    OptionExt,
    ResultExt,
    Snafu,
};

/// Schema entries by key. A key that starts with `$` is a
/// regex pattern.
type Elements = IndexMap<String, Entry>;

/// One schema entry with details, as `cargo.json` writes
/// it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Element
{
    description: Option<String>,
    contents:    ElementKind,
    values:      Option<Values>,
    // Read so that `deny_unknown_fields` accepts the key.
    #[expect(
        dead_code,
        reason = "the schema lists defaults that completion does not use"
    )]
    default:     Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Values
{
    NoDetail(Vec<String>),
    Detail(HashMap<String, String>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ElementKind
{
    Id(String),
    Complex(Elements),
}

/// A schema entry: a bare type name, or an [`Element`] with
/// details.
#[derive(Deserialize)]
#[serde(untagged)]
enum Entry
{
    Id(String),
    Complex(Element),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Data
{
    #[serde(rename = "$schema")]
    schema:     Elements,
    #[serde(rename = "$components")]
    components: Elements,
}

/// A defect in the bundled `cargo.json` schema.
#[derive(Debug, Snafu)]
pub enum SchemaError
{
    /// The schema is not valid JSON of the expected shape.
    #[snafu(display("the bundled schema is not valid"))]
    Json
    {
        /// The serde error.
        source: serde_json::Error,
    },
    /// A `$` key is not a valid regex.
    #[snafu(display("the schema key pattern `{pattern}` is not a valid regex"))]
    Pattern
    {
        /// The pattern after the `$`.
        pattern: String,
        /// The regex error.
        source:  regex::Error,
    },
    /// An entry names a component that the schema does not
    /// define before it.
    #[snafu(display("the schema names the unknown type `{name}`"))]
    UnknownType
    {
        /// The type name.
        name: String,
    },
}

static SCHEMA: LazyLock<Result<Parsed, SchemaError>> = LazyLock::new(parse_all);

/// Returns the bundled manifest schema, parsed on the first
/// call.
///
/// # Errors
///
/// Returns the [`SchemaError`] of the bundled schema on
/// every call when it is malformed. A unit test parses the
/// schema, so a release build cannot fail.
pub fn schema() -> Result<&'static Parsed, &'static SchemaError> { SCHEMA.as_ref() }

/// A parsed schema entry: its possible types, hover text
/// and known values.
#[derive(Debug)]
pub struct Val
{
    ty:     Vec<Types>,
    detail: Option<String>,
    values: Option<HashMap<String, Option<String>>>,
}

/// One type that a schema entry accepts.
#[derive(Debug)]
pub enum Types
{
    Map(Parsed),
    Element(Arc<Val>),
    String,
    Bool,
    Int,
    Object,
    Array(Box<Types>),
}

impl Types
{
    pub fn completions_path(
        &self,
        keys: &[String],
        index: usize,
        is_value: bool,
    ) -> Vec<String>
    {
        match self
        {
            Types::Element(val) =>
            {
                val.ty
                    .iter()
                    .flat_map(|v| v.completions_path(keys, index, is_value))
                    .collect()
            }
            Types::Map(v) => v.completions(keys, index, is_value).unwrap_or_default(),
            Types::Array(v) => v.completions_path(keys, index, is_value),
            _ => vec![],
        }
    }

    pub fn completions(
        &self,
        is_value: bool,
    ) -> Vec<String>
    {
        match is_value
        {
            true =>
            {
                match self
                {
                    Types::Element(val) =>
                    {
                        val.values
                            .as_ref()
                            .map_or(vec![], |v| v.keys().cloned().collect())
                    }
                    Types::Array(types) => types.completions(is_value),
                    _ => vec![],
                }
            }
            false =>
            {
                match self
                {
                    Types::Map(parsed) =>
                    {
                        parsed.entries.iter().filter_map(|v| v.0.as_str()).collect()
                    }
                    Types::Element(val) =>
                    {
                        val.ty
                            .iter()
                            .flat_map(|v| v.completions(is_value))
                            .collect()
                    }
                    Types::Array(types) => types.completions(is_value),
                    _ => vec![],
                }
            }
        }
    }

    pub fn search(
        &self,
        keys: &[String],
        index: usize,
        is_value: bool,
    ) -> Option<String>
    {
        match self
        {
            Types::Map(parsed) => parsed.get_detail(keys, index, is_value),
            Types::Array(types) => types.search(keys, index, is_value),
            Types::Element(val) => val.ty.iter().find_map(|v| v.search(keys, index, is_value)),
            _ => None,
        }
    }

    pub fn end_doc(&self) -> Option<String>
    {
        match self
        {
            Types::Element(val) =>
            {
                val.detail
                    .clone()
                    .or(val.ty.iter().find_map(|v| v.end_doc()))
            }
            Types::Array(types) => types.end_doc(),
            _ => None,
        }
    }

    pub fn end_doc_value(
        &self,
        key: &str,
    ) -> Option<String>
    {
        match self
        {
            Types::Element(val) =>
            {
                val.values
                    .as_ref()
                    .and_then(|v| v.get(key).cloned())
                    .flatten()
                    .or(val.ty.iter().find_map(|v| v.end_doc_value(key)))
            }
            Types::Array(types) => types.end_doc_value(key),
            _ => None,
        }
    }
}

fn parse_type(
    v: &str,
    info: &HashMap<String, Arc<Val>>,
) -> Result<Types, SchemaError>
{
    Ok(match v
    {
        "string" => Types::String,
        "bool" => Types::Bool,
        "object" => Types::Object,
        "number" => Types::Int,
        _ =>
        {
            match v.strip_prefix("array<").and_then(|v| v.strip_suffix('>'))
            {
                Some(inner) => Types::Array(Box::new(parse_type(inner, info)?)),
                None => Types::Element(info.get(v).context(UnknownTypeSnafu { name: v })?.clone()),
            }
        }
    })
}

/// Parses `a | b | c` into its types.
fn parse_types(
    id: &str,
    info: &HashMap<String, Arc<Val>>,
) -> Result<Vec<Types>, SchemaError>
{
    id.split('|').map(|v| parse_type(v.trim(), info)).collect()
}

/// Returns the key for a schema key, where a leading `$`
/// marks a regex.
fn parse_key(key: String) -> Result<Key, SchemaError>
{
    match key.strip_prefix('$')
    {
        Some(pattern) =>
        {
            Regex::new(pattern)
                .map(Key::Pattern)
                .context(PatternSnafu { pattern })
        }
        None => Ok(Key::Exact(key)),
    }
}

/// A schema key: one exact key, or a regex that matches
/// many keys.
#[derive(Debug)]
pub enum Key
{
    Exact(String),
    Pattern(Regex),
}

impl Key
{
    fn as_str(&self) -> Option<String>
    {
        match self
        {
            Key::Exact(k) => Some(k.to_owned()),
            _ => None,
        }
    }

    fn is_match(
        &self,
        key: &str,
    ) -> bool
    {
        match self
        {
            Key::Exact(k) => k == key,
            Key::Pattern(r) => r.is_match(key.as_bytes()),
        }
    }
}

/// One level of the parsed schema.
#[derive(Debug)]
pub struct Parsed
{
    /// The entries of this level, in schema order.
    pub entries: Vec<(Key, Arc<Val>)>,
}

impl Parsed
{
    /// Returns the completions for the last key of
    /// `keys[index..]`. With `is_value` the completions are
    /// values, and otherwise they are keys. Returns `None`
    /// when a key on the path is not in the schema.
    pub fn completions(
        &self,
        keys: &[String],
        index: usize,
        is_value: bool,
    ) -> Option<Vec<String>>
    {
        let key = keys.get(index)?;
        let last = index + 1 == keys.len();
        let item = &self.entries.iter().find(|(k, _)| k.is_match(key))?.1;
        match (last, is_value)
        {
            (true, true) =>
            {
                let mut v = item
                    .values
                    .as_ref()
                    .map(|v| v.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                v.extend(item.ty.iter().flat_map(|v| v.completions(is_value)));
                Some(v)
            }
            (true, false) =>
            {
                Some(
                    item.ty
                        .iter()
                        .flat_map(|v| v.completions(is_value))
                        .collect(),
                )
            }
            (false, _) =>
            {
                Some(
                    item.ty
                        .iter()
                        .flat_map(|v| v.completions_path(keys, index + 1, is_value))
                        .collect(),
                )
            }
        }
    }

    /// Returns the hover text for `keys[index..]`. With
    /// `is_value` the last entry of `keys` is a value, and
    /// the text describes that value.
    pub fn get_detail(
        &self,
        keys: &[String],
        index: usize,
        is_value: bool,
    ) -> Option<String>
    {
        let key = keys.get(index)?;
        let item = &self.entries.iter().find(|(k, _)| k.is_match(key))?.1;
        let remaining = keys.len() - index - 1;
        match (remaining, is_value)
        {
            (0, _) =>
            {
                item.detail
                    .clone()
                    .or_else(|| item.ty.iter().find_map(Types::end_doc))
            }
            (1, true) =>
            {
                let next = &keys[index + 1];
                item.values
                    .as_ref()
                    .and_then(|v| v.get(next).cloned().flatten())
                    .or_else(|| item.ty.iter().find_map(|v| v.end_doc_value(next)))
            }
            _ =>
            {
                item.ty
                    .iter()
                    .find_map(|v| v.search(keys, index + 1, is_value))
            }
        }
    }
}

/// Parses the bundled `cargo.json` schema.
fn parse_all() -> Result<Parsed, SchemaError>
{
    let data: Data = serde_json::from_str(include_str!("../cargo.json")).context(JsonSnafu)?;
    let mut map = HashMap::new();
    for (k, v) in data.components
    {
        let res = parse(v, &map)?;
        map.insert(k, Arc::new(res));
    }
    let mut entries = vec![];
    for (k, v) in data.schema
    {
        let entry = Arc::new(parse(v, &map)?);
        if k == "package"
        {
            map.insert("$package".to_string(), entry.clone());
        }
        entries.push((parse_key(k)?, entry));
    }
    Ok(Parsed { entries })
}

fn parse(
    t: Entry,
    info: &HashMap<String, Arc<Val>>,
) -> Result<Val, SchemaError>
{
    let t = match t
    {
        Entry::Id(id) =>
        {
            return Ok(Val {
                ty:     parse_types(&id, info)?,
                detail: None,
                values: None,
            });
        }
        Entry::Complex(t) => t,
    };
    let ty = match t.contents
    {
        ElementKind::Id(id) => parse_types(&id, info)?,
        ElementKind::Complex(map) =>
        {
            vec![Types::Map(Parsed {
                entries: map
                    .into_iter()
                    .map(|(k, v)| Ok((parse_key(k)?, Arc::new(parse(v, info)?))))
                    .collect::<Result<_, SchemaError>>()?,
            })]
        }
    };
    Ok(Val {
        ty,
        detail: t.description,
        values: t.values.map(|v| {
            match v
            {
                Values::NoDetail(v) => v.into_iter().map(|v| (v, None)).collect(),
                Values::Detail(v) => v.into_iter().map(|(k, v)| (k, Some(v))).collect(),
            }
        }),
    })
}

#[cfg(test)]
mod tests
{
    use super::*;

    fn keys(path: &[&str]) -> Vec<String> { path.iter().map(ToString::to_string).collect() }

    #[test]
    fn schema_completes_package_keys() -> Result<(), &'static SchemaError>
    {
        let completions = schema()?.completions(&keys(&["package"]), 0, false);
        assert!(completions.is_some_and(|v| v.contains(&"name".to_owned())));
        Ok(())
    }

    #[test]
    fn schema_describes_a_nested_key() -> Result<(), &'static SchemaError>
    {
        let detail = schema()?.get_detail(&keys(&["package", "name"]), 0, false);
        assert!(detail.is_some_and(|v| !v.is_empty()));
        Ok(())
    }

    #[test]
    fn schema_rejects_an_unknown_component()
    {
        let entry = Entry::Id("no-such-type".to_owned());
        assert!(matches!(
            parse(entry, &HashMap::new()),
            Err(SchemaError::UnknownType { name }) if name == "no-such-type"
        ));
    }
}
