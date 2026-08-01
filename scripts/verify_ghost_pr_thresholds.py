#!/usr/bin/env python3
"""Fail-closed acceptance gate for a Ghost PR benchmark summary.

The benchmark itself may be collected outside a developer workstation because
it needs a frozen GitHub cache. This verifier intentionally has no network
dependency: it accepts only the generated JSON summary and refuses a release
claim unless the pre-registered large-PR thresholds are present and met.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any


REQUIRED_THRESHOLDS = {
    "minimum_uplift": 0.20,
    "maximum_pair_cut_rate": 0.10,
    "minimum_stability_ari": 0.80,
    "minimum_effective_prs": 30,
}


def number(value: Any, field: str) -> float:
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        raise ValueError(f"{field} must be numeric")
    return float(value)


def verify(summary: dict[str, Any]) -> list[str]:
    decision = summary.get("decision")
    if not isinstance(decision, dict):
        return ["missing decision object"]
    thresholds = decision.get("thresholds")
    if thresholds != REQUIRED_THRESHOLDS:
        return [
            "benchmark thresholds do not exactly match the pre-registered "
            f"contract: expected {REQUIRED_THRESHOLDS}, got {thresholds}"
        ]
    try:
        uplift = number(decision.get("balanced_score_uplift"), "balanced_score_uplift")
        pair_cut = number(decision.get("pair_cut_rate"), "pair_cut_rate")
        ari = number(decision.get("stability_mean_ari"), "stability_mean_ari")
        effective = number(decision.get("effective_paired_prs"), "effective_paired_prs")
    except ValueError as error:
        return [str(error)]
    failures = []
    if uplift < REQUIRED_THRESHOLDS["minimum_uplift"]:
        failures.append(f"uplift {uplift:.3f} < 0.200")
    if pair_cut >= REQUIRED_THRESHOLDS["maximum_pair_cut_rate"]:
        failures.append(f"pair cut rate {pair_cut:.3f} is not < 0.100")
    if ari < REQUIRED_THRESHOLDS["minimum_stability_ari"]:
        failures.append(f"ARI {ari:.3f} < 0.800")
    if effective < REQUIRED_THRESHOLDS["minimum_effective_prs"]:
        failures.append(f"paired PR count {effective:.0f} < 30")
    if decision.get("verdict") != "PASS":
        failures.append(f"benchmark verdict is {decision.get('verdict')!r}, not 'PASS'")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("summary", type=Path)
    args = parser.parse_args()
    try:
        summary = json.loads(args.summary.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        print(f"Ghost PR gate: cannot load summary: {error}", file=sys.stderr)
        return 2
    failures = verify(summary)
    if failures:
        print("Ghost PR gate: FAILED", file=sys.stderr)
        for failure in failures:
            print(f"- {failure}", file=sys.stderr)
        return 1
    print("Ghost PR gate: PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
