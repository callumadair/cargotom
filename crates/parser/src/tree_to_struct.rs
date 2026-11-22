use std::sync::Arc;

use cargo_platform::Platform;

use crate::{
    Tree,
    structs::{
        deps::get_dependencies,
        feat::get_features,
    },
    toml::{
        Dependency,
        DependencyKind,
        Positioned,
        Toml,
    },
    tree::Value,
};

/// Returns the dependencies and features of `tree`.
/// `target` is the platform list of the `[target.*]` table
/// that holds `tree`. A key whose value has
/// the wrong shape, such as `target = "x"`, adds nothing.
pub fn to_struct(
    tree: &Tree,
    target: Arc<Vec<Positioned<Platform>>>,
) -> Toml
{
    let mut dep = vec![];
    let mut features = vec![];
    let mut tar = vec![];
    let mut workspace_deps = None;
    for value in &tree.nodes
    {
        match value.key.value.as_str()
        {
            "target" =>
            {
                let Some(targets) = value.value.as_tree()
                else
                {
                    continue;
                };
                for tree in &targets.nodes
                {
                    let Some(table) = tree.value.as_tree()
                    else
                    {
                        continue;
                    };
                    let key = &tree.key.value;
                    let range = tree.key.closest_range(tree.pos.end);
                    // A key that is still being typed is
                    // kept as a target name.
                    let platform = key
                        .parse::<Platform>()
                        .unwrap_or_else(|_| Platform::Name(key.to_string()));
                    let targets = Arc::new(vec![Positioned::new(range.start, range.end, platform)]);
                    tar.push(to_struct(table, targets));
                }
            }
            "features" => features.extend(get_features(&value.value)),
            "workspace" =>
            {
                workspace_deps
                    .get_or_insert_with(Vec::new)
                    .extend(workspace_dependencies(&value.value, &target))
            }
            key =>
            {
                if let Ok(kind) = key.parse::<DependencyKind>()
                {
                    dep.extend(get_dependencies(&value.value, kind, &target));
                }
            }
        }
    }
    let r = Toml {
        workspace: workspace_deps.is_some(),
        dependencies: workspace_deps.unwrap_or(dep),
        features,
    };
    tar.into_iter().fold(r, Toml::join)
}

/// Returns the `[workspace.dependencies]` entries of a
/// `[workspace]` table.
fn workspace_dependencies(
    value: &Value,
    targets: &Arc<Vec<Positioned<Platform>>>,
) -> Vec<Positioned<Dependency>>
{
    value
        .as_tree()
        .into_iter()
        .flat_map(|tree| &tree.nodes)
        .filter(|node| node.key.value == "dependencies")
        .flat_map(|node| get_dependencies(&node.value, DependencyKind::Normal, targets))
        .collect()
}

#[cfg(test)]
mod tests
{
    use rstest::rstest;

    use super::*;

    // A scalar where a table belongs panicked the analysis.
    #[rstest]
    #[case::scalar_target("target = \"x\"\n")]
    #[case::scalar_target_entry("[target]\nunix = 1\n")]
    #[case::scalar_workspace("workspace = 1\n")]
    #[case::scalar_dependencies("dependencies = \"x\"\n")]
    fn to_struct_skips_a_scalar_table(#[case] manifest: &str) -> Result<(), &'static str>
    {
        let dom = taplo::parser::parse(manifest).into_dom();
        let tree = Tree::from(dom.as_table().ok_or("the document root is not a table")?);
        assert!(to_struct(&tree, Arc::default()).dependencies.is_empty());
        Ok(())
    }
}
