#!/usr/bin/env bash
# Runs a true local Git + Codex capture proof. It intentionally does not use a
# mocked provider, a synthetic ledger, or an existing worktree.
set -euo pipefail

workspace_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
orkia_bin=${ORKIA_BIN:-"$workspace_root/target/debug/orkia"}
fixture_root="$workspace_root/scripts/fixtures/e2e"
test_root=$(mktemp -d "${TMPDIR:-/tmp}/orkia-codex-e2e.XXXXXX")
mirror_root="${test_root}-mirror.git"
cleanup() {
  rm -rf "$test_root" "$mirror_root"
}
trap cleanup EXIT

if ! command -v codex >/dev/null 2>&1; then
  echo "codex must be available on PATH" >&2
  exit 2
fi

cargo build --manifest-path "$workspace_root/Cargo.toml" -p orkia-cli -q
git -C "$test_root" init -b main
git -C "$test_root" config user.name 'Orkia Codex E2E'
git -C "$test_root" config user.email 'orkia-codex-e2e@example.test'
cp -R "$fixture_root/." "$test_root/"
git -C "$test_root" add src/lib.rs
git -C "$test_root" commit -m baseline
base_commit=$(git -C "$test_root" rev-parse main)

"$orkia_bin" --repository "$test_root" identity init --name 'Orkia Codex E2E'
"$orkia_bin" --repository "$test_root" session start \
  --origin codex --objective 'Add one captured semantic function'
"$orkia_bin" --repository "$test_root" session agent --provider codex -- \
  --json --sandbox workspace-write \
  'Use apply_patch to add exactly one public Rust function named semantic_feature to src/lib.rs. Do not run git commands, do not modify any other file, and do not explain; just make the edit.'
"$orkia_bin" --repository "$test_root" session checkpoint
"$orkia_bin" --repository "$test_root" ledger verify

plan_id=$(git -C "$test_root" for-each-ref --format='%(refname)' refs/orkia/plans \
  | sed -E 's#refs/orkia/plans/([^/]+)/.*#\1#' | sort -u | head -n 1)
test -n "$plan_id"
"$orkia_bin" --repository "$test_root" review project --plan "$plan_id"
"$orkia_bin" --repository "$test_root" ledger verify

# An unprotected feature branch exercises the successful policy path and
# records the signed integration decision before the ChangeSet forge gate is
# tested below.
"$orkia_bin" --repository "$test_root" integrate --plan "$plan_id" --branch feature
"$orkia_bin" --repository "$test_root" ledger verify

branch=$(git -C "$test_root" for-each-ref --format='%(refname:short)' \
  refs/heads/orkia/stack-pr | head -n 1)
test -n "$branch"
test "$(git -C "$test_root" rev-parse main)" = "$base_commit"
git -C "$test_root" show "$branch":src/lib.rs | grep -F 'pub fn semantic_feature'
! git -C "$test_root" diff --quiet main "$branch" -- src/lib.rs

# A projected but not forge-published stack must be rejected by the ChangeSet
# integration boundary. This exercises the coordinator path on the same real
# repository and proves that a local branch cannot masquerade as a published
# PR.
repository_id=$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1])))' \
  "$test_root/.git/orkia/repository.json")
stack_id=$(git -C "$test_root" for-each-ref --format='%(refname)' refs/orkia/stacks \
  | sed -E 's#refs/orkia/stacks/([^/]+)/.*#\1#' | sort -u | head -n 1)
test -n "$stack_id"
"$orkia_bin" --repository "$test_root" changeset create \
  --stack "$repository_id:$stack_id" \
  --repository-path "$repository_id=$test_root"
changeset_id=$(git -C "$test_root" for-each-ref --format='%(refname)' refs/orkia/changesets \
  | sed -E 's#refs/orkia/changesets/([^/]+)/.*#\1#' | sort -u | head -n 1)
test -n "$changeset_id"
status_json=$("$orkia_bin" --repository "$test_root" changeset status --id "$changeset_id" \
  --repository-path "$repository_id=$test_root")
printf '%s\n' "$status_json" | grep -F '"ready_for_integration": false'
if "$orkia_bin" --repository "$test_root" integrate --changeset "$changeset_id" \
  --repository-path "$repository_id=$test_root"; then
  echo "ChangeSet integration unexpectedly accepted an unpublished projection" >&2
  exit 1
fi

# A mirror gets the complete refs/orkia namespace but none of the convenient
# worktree cache. `review show` must therefore reconstruct from signed refs.
git clone --mirror "$test_root" "$mirror_root" >/dev/null
"$orkia_bin" --repository "$mirror_root" review show --plan "$plan_id" >/dev/null
git -C "$mirror_root" show-ref --verify --quiet "refs/orkia/plans/$plan_id/0"

echo "Codex/Git end-to-end proof passed: plan $plan_id, branch $branch"
