from __future__ import annotations

import importlib.util
from pathlib import Path


ROOT = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("gate", ROOT / "verify_ghost_pr_thresholds.py")
assert SPEC and SPEC.loader
gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate)


def summary(**values: float | str) -> dict:
    return {
        "decision": {
            "verdict": values.get("verdict", "PASS"),
            "balanced_score_uplift": values.get("uplift", 0.2),
            "pair_cut_rate": values.get("pair_cut", 0.099),
            "stability_mean_ari": values.get("ari", 0.8),
            "effective_paired_prs": values.get("effective", 30),
            "thresholds": gate.REQUIRED_THRESHOLDS,
        }
    }


def test_accepts_only_the_registered_boundary() -> None:
    assert gate.verify(summary()) == []


def test_rejects_the_current_commit_jaccard_result() -> None:
    failures = gate.verify(summary(verdict="NO PASS", uplift=0.0328, pair_cut=0.4222, ari=0.4405, effective=36))
    assert len(failures) == 4
    assert any("uplift" in failure for failure in failures)
    assert any("pair cut" in failure for failure in failures)
    assert any("ARI" in failure for failure in failures)
    assert any("verdict" in failure for failure in failures)
