use std::sync::Arc;

use cargo_platform::Platform;
use taplo::dom::node::TableKind;

use crate::{
    Tree,
    toml::{
        DepSource,
        Dependency,
        DependencyKind,
        OptionalKey,
        Positioned,
    },
    tree::{
        Value,
        str_to_positioned,
    },
};

/// Returns the entries of one dependency table. A value
/// that is not a table gives no entries.
pub(crate) fn get_dependencies(
    value: &Value,
    kind: DependencyKind,
    targets: &Arc<Vec<Positioned<Platform>>>,
) -> Vec<Positioned<Dependency>>
{
    let Some(tree) = value.as_tree()
    else
    {
        return vec![];
    };
    let mut out = vec![];
    for dep_tree in &tree.nodes
    {
        let mut dep = Dependency {
            name: dep_tree.key.to_positioned(dep_tree.pos.end),
            package: None,
            kind,
            expanded: false,
            source: DepSource::None,
            features: Positioned::new(0, 0, vec![]),
            optional: None,
            target: targets.clone(),
            default_features: None,
            features_key_range: None,
            typing_keys: vec![],
        };
        match &dep_tree.value
        {
            Value::Tree { value } =>
            {
                dependency_tree_format_parser(value, &mut dep);
                dep.expanded = value.kind == TableKind::Inline;
            }
            Value::String { value, range } =>
            {
                dep.source = DepSource::Version {
                    value:    OptionalKey::no_key(str_to_positioned(value, range)),
                    registry: None,
                };
            }
            Value::NoContent =>
            {}
            _ => continue,
        }
        out.push(Positioned::new(dep_tree.pos.start, dep_tree.pos.end, dep));
    }
    out
}

/// Sets one string key of a dependency source.
type SourceSetter = fn(&mut DepSource, Positioned<String>);

fn dependency_tree_format_parser(
    value: &Tree,
    dep: &mut Dependency,
)
{
    for tree_value in &value.nodes
    {
        let key = tree_value.key.value.as_str();
        let key_range = tree_value.key.closest_range(tree_value.pos.end);
        let setter: Option<SourceSetter> = match key
        {
            "registry" => Some(DepSource::set_registry),
            "git" => Some(DepSource::set_git),
            "path" => Some(DepSource::set_path),
            "branch" => Some(DepSource::set_branch),
            "tag" => Some(DepSource::set_tag),
            "rev" => Some(DepSource::set_rev),
            _ => None,
        };
        if let Some(setter) = setter
        {
            if let Some(value) = tree_value.value.as_str()
            {
                setter(&mut dep.source, value);
            }
            continue;
        }
        match key
        {
            // A version key without a string still marks
            // where the value goes.
            "version" =>
            {
                let value = tree_value.value.as_str().unwrap_or_else(|| {
                    Positioned::new(key_range.end, key_range.end, String::new())
                });
                dep.source
                    .set_version(OptionalKey::with_key(key_range, value));
            }
            "features" =>
            {
                dep.features_key_range = Some(key_range);
                if let Value::Array { value, range } = &tree_value.value
                {
                    let range = tree_value.pos.join(range);
                    dep.features = Positioned::new(
                        range.start,
                        range.end,
                        value.iter().filter_map(Value::as_str).collect(),
                    );
                }
            }
            "default-features" | "default_features" =>
            {
                if let Some(value) = tree_value.value.as_bool()
                {
                    dep.default_features = Some(value);
                }
            }
            "package" =>
            {
                if let Some(value) = tree_value.value.as_str()
                {
                    dep.package = Some(value);
                }
            }
            "optional" =>
            {
                if let Some(value) = tree_value.value.as_bool()
                {
                    dep.optional = Some(value);
                }
            }
            "workspace" =>
            {
                dep.source.set_workspace(
                    tree_value
                        .value
                        .range()
                        .map_or(tree_value.pos, |v| v.join(&tree_value.pos)),
                )
            }
            _ =>
            {
                dep.typing_keys
                    .push(tree_value.key.to_positioned(tree_value.pos.end))
            }
        }
    }
}
