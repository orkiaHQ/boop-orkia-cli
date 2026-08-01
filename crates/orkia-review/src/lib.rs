//! Deterministic review planning from captured causal evidence.

use orkia_model::{
    AtomDependency, AtomId, ChangeAtom, DependencyKind, OrkiaError, PlanId, PlanStatus, Result,
    ReviewPlan, ReviewUnit, ReviewUnitId,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct PlanningInput {
    pub checkpoint: String,
    pub atoms: Vec<ChangeAtom>,
    pub dependencies: Vec<AtomDependency>,
    pub coverage_milli: u16,
    pub minimum_coverage_milli: u16,
    pub minimum_confidence_milli: u16,
    pub source_events: BTreeSet<orkia_model::EventId>,
}

#[derive(Clone, Debug)]
pub enum ReviewerCorrection {
    Merge {
        units: BTreeSet<ReviewUnitId>,
        reason: String,
    },
    Split {
        unit: ReviewUnitId,
        groups: Vec<BTreeSet<AtomId>>,
        reason: String,
    },
}

pub fn plan(input: PlanningInput) -> ReviewPlan {
    let confidence = confidence(&input.dependencies, input.atoms.len());
    if input.coverage_milli < input.minimum_coverage_milli
        || confidence < input.minimum_confidence_milli
    {
        return one_unit_plan(&input, confidence);
    }
    let components = hard_components(&input.atoms, &input.dependencies);
    let atom_to_unit: BTreeMap<_, _> = components
        .iter()
        .enumerate()
        .flat_map(|(index, atoms)| atoms.iter().map(move |atom| (atom.clone(), index)))
        .collect();
    let mut units: Vec<ReviewUnit> = components
        .into_iter()
        .map(|atoms| ReviewUnit {
            id: deterministic_unit(&atoms),
            title: title_for(&atoms, &input.atoms),
            atoms,
            depends_on: BTreeSet::new(),
            confidence_milli: confidence,
        })
        .collect();
    for dep in &input.dependencies {
        let Some(&from) = atom_to_unit.get(&dep.from) else {
            continue;
        };
        let Some(&to) = atom_to_unit.get(&dep.to) else {
            continue;
        };
        if from != to
            && matches!(
                dep.kind,
                DependencyKind::Hard
                    | DependencyKind::Import
                    | DependencyKind::Test
                    | DependencyKind::Configuration
            )
        {
            let to_id = units[to].id.clone();
            units[from].depends_on.insert(to_id);
        }
    }
    units.sort_by(|a, b| a.id.cmp(&b.id));
    let atom_paths = input
        .atoms
        .iter()
        .map(|atom| (atom.id.clone(), atom.path.clone()))
        .collect();
    ReviewPlan {
        schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
        id: deterministic_plan(&input.checkpoint, 0),
        revision: 0,
        source_checkpoint: input.checkpoint,
        units,
        atom_paths,
        atoms: input.atoms,
        coverage_milli: input.coverage_milli,
        status: PlanStatus::Proposed,
        created_from: input.source_events,
    }
}

pub fn apply_correction(plan: &ReviewPlan, correction: ReviewerCorrection) -> Result<ReviewPlan> {
    let mut next = plan.clone();
    next.revision += 1;
    next.status = PlanStatus::Proposed;
    match correction {
        ReviewerCorrection::Merge { units, reason: _ } => {
            if units.len() < 2 {
                return Err(OrkiaError::Invalid(
                    "a merge needs at least two review units".into(),
                ));
            }
            let selected: Vec<_> = next
                .units
                .iter()
                .filter(|u| units.contains(&u.id))
                .cloned()
                .collect();
            if selected.len() != units.len() {
                return Err(OrkiaError::NotFound("review unit for merge".into()));
            }
            let atoms = selected
                .iter()
                .flat_map(|u| u.atoms.iter().cloned())
                .collect::<BTreeSet<_>>();
            let merged_id = deterministic_unit(&atoms);
            let dependencies = selected
                .iter()
                .flat_map(|u| u.depends_on.iter().cloned())
                .filter(|d| !units.contains(d))
                .collect();
            let confidence_milli = selected
                .iter()
                .map(|u| u.confidence_milli)
                .min()
                .unwrap_or(0);
            next.units.retain(|u| !units.contains(&u.id));
            next.units.push(ReviewUnit {
                id: merged_id.clone(),
                title: "Reviewer merged review units".into(),
                atoms,
                depends_on: dependencies,
                confidence_milli,
            });
            for unit in &mut next.units {
                if unit.depends_on.iter().any(|id| units.contains(id)) {
                    unit.depends_on.retain(|id| !units.contains(id));
                    unit.depends_on.insert(merged_id.clone());
                }
            }
        }
        ReviewerCorrection::Split {
            unit,
            groups,
            reason: _,
        } => {
            let position = next
                .units
                .iter()
                .position(|u| u.id == unit)
                .ok_or_else(|| OrkiaError::NotFound("review unit for split".into()))?;
            let original = next.units.remove(position);
            let partition = groups
                .iter()
                .flat_map(|group| group.iter().cloned())
                .collect::<BTreeSet<_>>();
            if groups.len() < 2
                || groups.iter().any(BTreeSet::is_empty)
                || partition != original.atoms
            {
                return Err(OrkiaError::Invalid(
                    "split groups must partition the original atoms".into(),
                ));
            }
            let new_ids: Vec<_> = groups.iter().map(deterministic_unit).collect();
            for (index, atoms) in groups.into_iter().enumerate() {
                next.units.push(ReviewUnit {
                    id: new_ids[index].clone(),
                    title: format!("{} ({})", original.title, index + 1),
                    atoms,
                    depends_on: original.depends_on.clone(),
                    confidence_milli: original.confidence_milli,
                });
            }
            for review in &mut next.units {
                if review.depends_on.remove(&unit) {
                    review.depends_on.extend(new_ids.iter().cloned());
                }
            }
        }
    }
    next.id = deterministic_plan(&next.source_checkpoint, next.revision);
    next.units.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(next)
}

fn one_unit_plan(input: &PlanningInput, confidence: u16) -> ReviewPlan {
    let atoms = input.atoms.iter().map(|a| a.id.clone()).collect();
    let atom_paths = input
        .atoms
        .iter()
        .map(|atom| (atom.id.clone(), atom.path.clone()))
        .collect();
    ReviewPlan {
        schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
        id: deterministic_plan(&input.checkpoint, 0),
        revision: 0,
        source_checkpoint: input.checkpoint.clone(),
        units: vec![ReviewUnit {
            id: deterministic_unit(&atoms),
            title: "Single review: insufficient causal coverage".into(),
            atoms,
            depends_on: BTreeSet::new(),
            confidence_milli: confidence,
        }],
        atom_paths,
        atoms: input.atoms.clone(),
        coverage_milli: input.coverage_milli,
        status: PlanStatus::Proposed,
        created_from: input.source_events.clone(),
    }
}
fn hard_components(atoms: &[ChangeAtom], dependencies: &[AtomDependency]) -> Vec<BTreeSet<AtomId>> {
    let ids: BTreeSet<_> = atoms.iter().map(|a| a.id.clone()).collect();
    let mut adj: BTreeMap<AtomId, BTreeSet<AtomId>> = ids
        .iter()
        .cloned()
        .map(|id| (id, BTreeSet::new()))
        .collect();
    for dep in dependencies
        .iter()
        .filter(|d| matches!(d.kind, DependencyKind::Hard))
    {
        if let Some(edges) = adj.get_mut(&dep.from) {
            edges.insert(dep.to.clone());
        }
        if let Some(edges) = adj.get_mut(&dep.to) {
            edges.insert(dep.from.clone());
        }
    }
    let mut left = ids;
    let mut parts = Vec::new();
    while let Some(start) = left.iter().next().cloned() {
        let mut queue = VecDeque::from([start]);
        let mut part = BTreeSet::new();
        while let Some(current) = queue.pop_front() {
            if !left.remove(&current) {
                continue;
            }
            part.insert(current.clone());
            if let Some(edges) = adj.get(&current) {
                queue.extend(edges.iter().cloned());
            }
        }
        parts.push(part);
    }
    parts
}
fn confidence(deps: &[AtomDependency], atom_count: usize) -> u16 {
    if atom_count == 0 {
        return 0;
    }
    if deps.is_empty() {
        return 850;
    } else {
        (deps.iter().map(|d| d.confidence_milli as u32).sum::<u32>() / deps.len() as u32) as u16
    }
}
fn deterministic_unit(atoms: &BTreeSet<AtomId>) -> ReviewUnitId {
    let input = atoms
        .iter()
        .map(|id| id.0.to_string())
        .collect::<Vec<_>>()
        .join("|");
    let digest = Sha256::digest(input.as_bytes());
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    ReviewUnitId(Uuid::from_bytes(bytes))
}
fn deterministic_plan(checkpoint: &str, revision: u32) -> PlanId {
    let digest = Sha256::digest(format!("{checkpoint}:{revision}").as_bytes());
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    PlanId(Uuid::from_bytes(bytes))
}
fn title_for(atoms: &BTreeSet<AtomId>, all: &[ChangeAtom]) -> String {
    let mut paths: Vec<_> = all
        .iter()
        .filter(|atom| atoms.contains(&atom.id))
        .map(|atom| atom.path.clone())
        .collect();
    paths.sort();
    paths
        .first()
        .map(|path| format!("Review {path}"))
        .unwrap_or_else(|| "Review change".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_model::{AtomKind, EventId};
    fn atom() -> ChangeAtom {
        ChangeAtom {
            id: AtomId::new(),
            kind: AtomKind::Symbol,
            path: "a.rs".into(),
            symbol: None,
            start_line: 1,
            end_line: 1,
            content_hash: "a".into(),
            source_events: BTreeSet::new(),
        }
    }
    #[test]
    fn incomplete_capture_never_splits() {
        let input = PlanningInput {
            checkpoint: "x".into(),
            atoms: vec![atom(), atom()],
            dependencies: vec![],
            coverage_milli: 0,
            minimum_coverage_milli: 950,
            minimum_confidence_milli: 800,
            source_events: BTreeSet::<EventId>::new(),
        };
        assert_eq!(plan(input).units.len(), 1);
    }
}
