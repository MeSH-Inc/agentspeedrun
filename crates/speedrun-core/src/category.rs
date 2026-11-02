use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Trust;

/// A versioned ruleset. Runs pin `(id, version)` at start so later edits to
/// a category never change how an old run is judged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Category {
    pub id: String,
    pub version: u32,
    pub name: String,
    pub description: String,
    /// The run completes when every one of these counts.
    pub milestones: Vec<MilestoneTemplate>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MilestoneTemplate {
    pub key: String,
    pub title: String,
    pub description: String,
    pub required_trust: Trust,
    /// Evidence kinds that satisfy this milestone, as a hint to users and agents.
    #[serde(default)]
    pub suggested_evidence: Vec<String>,
}

const BUILTIN: &[&str] = &[
    include_str!("../../../categories/any-percent.json"),
    include_str!("../../../categories/hundred-percent.json"),
    include_str!("../../../categories/first-revenue.json"),
];

pub const DEFAULT_CATEGORY: &str = "any-percent";

/// Categories shipped with the binary.
pub fn builtin() -> Vec<Category> {
    BUILTIN
        .iter()
        .map(|s| serde_json::from_str(s).expect("builtin category JSON is valid"))
        .collect()
}

/// Latest version of a category by id, or an exact version if given.
pub fn find<'a>(all: &'a [Category], id: &str, version: Option<u32>) -> Option<&'a Category> {
    all.iter()
        .filter(|c| c.id == id && version.is_none_or(|v| c.version == v))
        .max_by_key(|c| c.version)
}

/// The sha256 of a category's canonical JSON: the `ruleset_digest` a run
/// records, so a published run names its rules even after `categories/` changes.
pub fn digest(c: &Category) -> Result<String, serde_json::Error> {
    serde_json::to_value(c).map(|value| crate::hash::canonical_digest(&value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_parse_and_have_unique_keys() {
        let all = builtin();
        assert_eq!(all.len(), 3);
        for c in &all {
            let mut keys: Vec<_> = c.milestones.iter().map(|m| &m.key).collect();
            keys.sort();
            keys.dedup();
            assert_eq!(keys.len(), c.milestones.len(), "duplicate key in {}", c.id);
        }
        assert!(find(&all, DEFAULT_CATEGORY, None).is_some());
    }
}
