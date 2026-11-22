use cargo::workspace::FeatureValue;

use crate::{
    toml::{Feature, Positioned},
    tree::Value,
};

pub(crate) fn get_features(value: &Value) -> Option<Vec<Positioned<Feature>>> {
    let tree = value.as_tree()?;
    let mut out = vec![];
    for tree_value in tree.nodes.iter() {
        let name = tree_value.key.to_positioned(tree_value.pos.end);
        let range = tree_value.key.closest_range(tree_value.pos.end);

        match &tree_value.value {
            Value::Array { value, .. } => {
                let args = value
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(|v| Positioned::new(v.start, v.end, FeatureValue::new(v.data.as_str().into())))
                    .collect::<Vec<_>>();
                let max = args
                    .iter()
                    .map(|v| v.end)
                    .max()
                    .unwrap_or(tree_value.key.closest_range(tree_value.pos.end).end);
                out.push(Positioned {
                    start: range.start,
                    end: range.end.max(max),
                    data: Feature { name, args },
                });
            }
            Value::NoContent => out.push(Positioned {
                start: range.start,
                end: range.end,
                data: Feature { name, args: vec![] },
            }),
            _ => continue,
        };
    }
    Some(out)
}
