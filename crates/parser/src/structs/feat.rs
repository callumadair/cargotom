use cargo::workspace::FeatureValue;

use crate::{
    toml::{
        Feature,
        Positioned,
    },
    tree::Value,
};

/// Returns the entries of a `[features]` table. A value
/// that is not a table gives no entries, and an entry that
/// is not an array is skipped.
pub(crate) fn get_features(value: &Value) -> Vec<Positioned<Feature>>
{
    let Some(tree) = value.as_tree()
    else
    {
        return vec![];
    };
    let mut out = vec![];
    for tree_value in &tree.nodes
    {
        let name = tree_value.key.to_positioned(tree_value.pos.end);
        let range = tree_value.key.closest_range(tree_value.pos.end);
        let args = match &tree_value.value
        {
            Value::Array { value, .. } =>
            {
                value
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|v| {
                        Positioned::new(v.start, v.end, FeatureValue::new(v.data.as_str().into()))
                    })
                    .collect::<Vec<_>>()
            }
            Value::NoContent => vec![],
            _ => continue,
        };
        let end = args.iter().map(|v| v.end).fold(range.end, u32::max);
        out.push(Positioned::new(range.start, end, Feature { name, args }));
    }
    out
}
