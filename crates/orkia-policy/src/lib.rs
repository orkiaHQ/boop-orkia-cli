//! Repository-versioned publication and integration policy.

use orkia_model::{OrkiaError, RepositoryPolicy, Result, ReviewPlan, ValidationResult};
use orkia_ports::ValidationExecutor;

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
    fn protected_branch_needs_approval() {
        let mut policy = RepositoryPolicy::default();
        policy.minimum_coverage_milli = 0;
        policy.minimum_confidence_milli = 0;
        let plan = ReviewPlan {
            id: orkia_model::PlanId::new(),
            revision: 0,
            source_checkpoint: "x".into(),
            units: vec![],
            atom_paths: Default::default(),
            coverage_milli: 1000,
            status: orkia_model::PlanStatus::Proposed,
            created_from: Default::default(),
        };
        assert!(evaluate(&policy, &plan, &[], 0, "main").is_err());
    }
}
