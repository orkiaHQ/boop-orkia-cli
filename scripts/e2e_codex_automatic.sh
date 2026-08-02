#!/usr/bin/env bash
# Runs the Phase 0 reference flow with a real Codex process.  The script does
# not invoke `orkia session start`, `checkpoint`, `review`, or `changeset`:
# Codex hooks must create the complete signed state automatically.
set -euo pipefail

workspace_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
orkia_bin=${ORKIA_BIN:-"$workspace_root/target/debug/orkia"}
fixture_root="$workspace_root/scripts/fixtures/e2e"
test_root=$(mktemp -d "${TMPDIR:-/tmp}/orkia-codex-automatic.XXXXXX")
codex_output=$(mktemp "${TMPDIR:-/tmp}/orkia-codex-automatic-output.XXXXXX")

cleanup() {
  rm -rf -- "$test_root" "$codex_output"
}
trap cleanup EXIT

command -v codex >/dev/null 2>&1 || {
  echo "codex must be available on PATH" >&2
  exit 2
}

cargo build --manifest-path "$workspace_root/Cargo.toml" -p orkia-cli -q
git -C "$test_root" init -b main >/dev/null
git -C "$test_root" config user.name 'Orkia automatic Codex E2E'
git -C "$test_root" config user.email 'orkia-automatic@example.test'
cp -R "$fixture_root/." "$test_root/"
git -C "$test_root" add src/lib.rs
git -C "$test_root" commit -m baseline >/dev/null
base_commit=$(git -C "$test_root" rev-parse HEAD)

"$orkia_bin" --repository "$test_root" init \
  --name 'Orkia automatic Codex E2E' --agent codex >/dev/null

# Keep Codex's JSON output outside the repository.  An output file inside the
# worktree would correctly be treated as an unknown write and block the plan.
(
  cd "$test_root"
  codex exec --json --sandbox workspace-write \
    'Use apply_patch to add exactly one public Rust function named automatic_phase0_feature to src/lib.rs. Do not run git commands and do not modify any other file.'
) >"$codex_output"

"$orkia_bin" --repository "$test_root" ledger verify >/dev/null

plan_id=$(git -C "$test_root" for-each-ref --format='%(refname)' refs/orkia/plans \
  | sed -E 's#refs/orkia/plans/([^/]+)/.*#\1#' | sort -u | head -n 1)
stack_pr_ref=$(git -C "$test_root" for-each-ref --format='%(refname)' refs/orkia/stack-prs \
  | sort | head -n 1)
stack_ref=$(git -C "$test_root" for-each-ref --format='%(refname)' refs/orkia/stacks \
  | sort | head -n 1)
changeset_ref=$(git -C "$test_root" for-each-ref --format='%(refname)' refs/orkia/changesets \
  | sort | head -n 1)
test -n "$plan_id" -a -n "$stack_pr_ref" -a -n "$stack_ref" -a -n "$changeset_ref"

stack_pr_object=$(git -C "$test_root" rev-parse "$stack_pr_ref")
git -C "$test_root" cat-file blob "$stack_pr_object" \
  | python3 -c 'import json, sys; value=json.load(sys.stdin); validations=value.get("validations", []); intent=value.get("intent"); assert intent and intent.get("kind") == "intent", value; assert any(item.get("command") == "git diff --check" and item.get("passed") for item in validations), value'

test "$(git -C "$test_root" rev-parse HEAD)" = "$base_commit"
test "$(git -C "$test_root" diff --name-only "$base_commit" | sort)" = "src/lib.rs"

echo "automatic Codex Phase 0 E2E passed: plan $plan_id, stack PR $stack_pr_ref, ChangeSet $changeset_ref"
