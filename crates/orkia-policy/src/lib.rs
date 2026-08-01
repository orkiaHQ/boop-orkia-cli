//! Repository-versioned publication and integration policy.

use orkia_model::{OrkiaError, RepositoryPolicy, Result, ReviewPlan, ValidationResult};
use orkia_ports::ValidationExecutor;

/// Parses repository policy bytes without choosing a storage mechanism. File,
/// Git and network adapters belong in the composition roots, so this domain
/// crate remains usable in a pure contract test and in a server that sources
/// policy from any durable store.
pub fn parse(content: &str) -> Result<RepositoryPolicy> {
    toml::from_str(content).map_err(|error| OrkiaError::Invalid(format!("policy: {error}")))
}

pub fn evaluate(
    policy: &RepositoryPolicy,
    plan: &ReviewPlan,
    validations: &[ValidationResult],
    approvals: u8,
    branch: &str,
) -> Result<()> {
    if plan.coverage_milli < policy.minimum_coverage_milli {
        return Err(OrkiaError::Policy(format!(
            "causal coverage {} is below required {}",
            plan.coverage_milli, policy.minimum_coverage_milli
        )));
    }
    if plan
        .units
        .iter()
        .any(|unit| unit.confidence_milli < policy.minimum_confidence_milli)
    {
        return Err(OrkiaError::Policy(
            "review plan has an under-confident unit".into(),
        ));
    }
    if validations.iter().any(|validation| !validation.passed) {
        return Err(OrkiaError::Policy("a required validation failed".into()));
    }
    if policy.protected_branches.contains(branch) && approvals < policy.required_approvals {
        return Err(OrkiaError::Policy(format!(
            "{branch} needs {} approvals",
            policy.required_approvals
        )));
    }
    if policy.protected_branches.contains(branch)
        && !matches!(plan.status, orkia_model::PlanStatus::Approved)
    {
        return Err(OrkiaError::Policy(
            "a protected branch requires an approved signed review plan".into(),
        ));
    }
    Ok(())
}

pub fn run_and_evaluate(
    executor: &dyn ValidationExecutor,
    policy: &RepositoryPolicy,
    plan: &ReviewPlan,
    approvals: u8,
    branch: &str,
) -> Result<Vec<ValidationResult>> {
    let results = executor.execute(policy)?;
    evaluate(policy, plan, &results, approvals, branch)?;
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versioned_policy_without_accessing_storage() {
        let policy = parse(
            r#"
                protected_branches = ["main"]
                validation_commands = ["cargo test"]
                minimum_coverage_milli = 950
                minimum_confidence_milli = 800
                required_approvals = 2
                required_checks = ["orkia/integrate", "security"]
            "#,
        )
        .unwrap();
        assert_eq!(policy.required_approvals, 2);
        assert!(policy.required_checks.contains("security"));
    }

    #[test]
    fn protected_branch_needs_approval() {
        let policy = RepositoryPolicy {
            minimum_coverage_milli: 0,
            minimum_confidence_milli: 0,
            ..RepositoryPolicy::default()
        };
        let plan = ReviewPlan {
            schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
            id: orkia_model::PlanId::new(),
            revision: 0,
            source_checkpoint: "x".into(),
            policy_digest: None,
            units: vec![],
            atom_paths: Default::default(),
            atoms: vec![],
            coverage_milli: 1000,
            status: orkia_model::PlanStatus::Proposed,
            created_from: Default::default(),
        };
        assert!(evaluate(&policy, &plan, &[], 0, "main").is_err());
    }

    #[test]
    fn protected_branch_accepts_an_approved_signed_plan_with_required_approval() {
        let policy = RepositoryPolicy {
            minimum_coverage_milli: 0,
            minimum_confidence_milli: 0,
            required_approvals: 1,
            ..RepositoryPolicy::default()
        };
        let plan = ReviewPlan {
            schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
            id: orkia_model::PlanId::new(),
            revision: 1,
            source_checkpoint: "x".into(),
            policy_digest: None,
            units: vec![],
            atom_paths: Default::default(),
            atoms: vec![],
            coverage_milli: 1000,
            status: orkia_model::PlanStatus::Approved,
            created_from: Default::default(),
        };
        assert!(evaluate(&policy, &plan, &[], 1, "main").is_ok());
    }
}
