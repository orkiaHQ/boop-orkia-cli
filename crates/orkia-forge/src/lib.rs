//! Forge-neutral review projection planning.

use orkia_model::{ForgeReview, OrkiaError, Result, ReviewPlan};
use std::collections::{BTreeMap, BTreeSet};

pub fn projections(plan: &ReviewPlan, base_branch: &str) -> Result<Vec<ForgeReview>> {
    let by_id: BTreeMap<_, _> = plan.units.iter().map(|unit| (unit.id.clone(), unit)).collect(); let mut remaining: BTreeSet<_> = by_id.keys().cloned().collect(); let mut projected = BTreeSet::new(); let mut output = Vec::new();
    while !remaining.is_empty() {
        let ready: Vec<_> = remaining.iter().filter(|id| by_id[*id].depends_on.iter().all(|dependency| projected.contains(dependency))).cloned().collect();
        if ready.is_empty() { return Err(OrkiaError::Conflict("review plan has a dependency cycle".into())); }
        for id in ready { let unit = by_id[&id]; let parent = unit.depends_on.iter().max().and_then(|dependency| output.iter().find(|review: &&ForgeReview| review.unit == *dependency)).map(|review| review.branch.clone()).unwrap_or_else(|| base_branch.into()); let branch = format!("orkia/{}/r{}", plan.id.0.simple(), unit.id.0.simple()); output.push(ForgeReview { unit: id.clone(), branch, base: parent, title: unit.title.clone(), body: format!("Orkia review unit {} from signed plan {} revision {}.", id.0, plan.id.0, plan.revision) }); remaining.remove(&id); projected.insert(id); }
    }
    Ok(output)
}
