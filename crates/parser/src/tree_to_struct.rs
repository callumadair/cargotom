use std::sync::Arc;

use cargo_platform::Platform;

use crate::{
    structs::{deps::get_dependencies, feat::get_features},
    toml::{Dependency, DependencyKind, Positioned, Toml},
    tree::Value,
    Tree,
};

pub fn to_struct(tree: &Tree, target: Arc<Vec<Positioned<Platform>>>) -> Toml {
    let mut dep = vec![];
    let mut features = vec![];
    let mut tar = vec![];
    let mut workspace_deps = None;
    for value in tree.nodes.iter() {
        match value.key.value.as_str() {
            "target" => {
                for tree in value.value.as_tree().unwrap().nodes.iter() {
                    let key = &tree.key.value;
                    let range = tree.key.closest_range(tree.pos.end);
                    // A key that is still being typed is kept as a target name.
                    let platform = key
                        .parse::<Platform>()
                        .unwrap_or_else(|_| Platform::Name(key.to_string()));
                    let targets = Arc::new(vec![Positioned::new(range.start, range.end, platform)]);
                    tar.push(to_struct(tree.value.as_tree().unwrap(), targets));
                }
            }
            "features" => {
                features.extend(get_features(&value.value).unwrap_or_default());
            }
            "workspace" => {
                workspace_deps
                    .get_or_insert_with(Vec::new)
                    .extend(workspace_dependencies(&value.value, target.clone()).unwrap_or_default());
            }
            key => {
                if let Ok(kind) = key.parse::<DependencyKind>() {
                    dep.extend(get_dependencies(&value.value, kind, target.clone()).unwrap_or_default());
                }
            }
        }
    }
    let workspace = workspace_deps.is_some();
    let tar = tar.into_iter().reduce(|acc, i| acc.join(i));
    let r = Toml {
        workspace,
        dependencies: workspace_deps.unwrap_or(dep),
        features,
    };
    match tar {
        Some(tar) => r.join(tar),
        None => r,
    }
}

/// Returns the `[workspace.dependencies]` entries of a `[workspace]` table.
fn workspace_dependencies(
    value: &Value,
    targets: Arc<Vec<Positioned<Platform>>>,
) -> Option<Vec<Positioned<Dependency>>> {
    let tree = value.as_tree()?;
    let deps = tree
        .nodes
        .iter()
        .filter(|node| node.key.value == "dependencies")
        .flat_map(|node| {
            get_dependencies(&node.value, DependencyKind::Normal, targets.clone()).unwrap_or_default()
        })
        .collect();
    Some(deps)
}
