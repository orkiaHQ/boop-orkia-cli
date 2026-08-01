#!/usr/bin/env bash
# Exercises Orkia's real GitHub transport against a temporary repository.
# The repository is kept by default so the result can be inspected in the
# GitHub UI. Set ORKIA_CLEANUP_GITHUB_REPO=1 only when deletion is explicitly
# desired. A GitHub App installation token is not available on every developer
# workstation, so the same adapter injection point accepts a short-lived test
# token from `gh auth token` for this proof.
set -euo pipefail

workspace_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
orkia_bin=${ORKIA_BIN:-"$workspace_root/target/debug/orkia"}
owner=${GH_REPO_OWNER:-$(gh api user --jq .login)}
name=${GH_REPO_NAME:-"orkia-protected-e2e-$(date +%Y%m%d%H%M%S)"}
full_repo="$owner/$name"
repo=$(mktemp -d "${TMPDIR:-/tmp}/orkia-github-e2e.XXXXXX")
cleanup() {
  rm -rf "$repo"
  if [[ "${ORKIA_CLEANUP_GITHUB_REPO:-0}" == "1" ]]; then
    gh repo delete "$full_repo" --yes >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

command -v gh >/dev/null 2>&1 || { echo "gh is required" >&2; exit 2; }
gh auth status >/dev/null
cargo build --manifest-path "$workspace_root/Cargo.toml" -q -p orkia-cli

# GitHub Free exposes branch protection for public repositories.  The
# repository contains only the temporary fixture and can be made private by
# callers using a plan that supports protected private branches.
gh repo create "$full_repo" --public --description "Orkia protected-branch E2E" >/dev/null
github_token=${ORKIA_GITHUB_INSTALLATION_TOKEN:-$(gh auth token)}
git -C "$repo" init -b main >/dev/null
git -C "$repo" config user.name 'Orkia GitHub E2E'
git -C "$repo" config user.email 'orkia-github-e2e@example.test'
mkdir -p "$repo/src"
printf 'pub fn baseline() {}\n' >"$repo/src/lib.rs"
git -C "$repo" add src/lib.rs
git -C "$repo" commit -m baseline >/dev/null
git -C "$repo" remote add origin "https://x-access-token:$github_token@github.com/$full_repo.git"
git -C "$repo" push -u origin main >/dev/null

"$orkia_bin" --repository "$repo" identity init --name 'Orkia GitHub E2E' >/dev/null
"$orkia_bin" --repository "$repo" session start --origin codex \
  --objective 'publish one protected semantic review' >/dev/null
"$orkia_bin" --repository "$repo" session run -- sh -c \
  "printf 'pub fn baseline() {}\\npub fn github_feature() {}\\n' > src/lib.rs" >/dev/null
"$orkia_bin" --repository "$repo" session checkpoint >/dev/null
plan_id=$(git -C "$repo" for-each-ref --format='%(refname)' refs/orkia/plans \
  | sed -E 's#refs/orkia/plans/([^/]+)/.*#\1#' | sort -u | head -n 1)
test -n "$plan_id"
"$orkia_bin" --repository "$repo" review approve --plan "$plan_id" >/dev/null
"$orkia_bin" --repository "$repo" review project --plan "$plan_id" >/dev/null

export ORKIA_GITHUB_INSTALLATION_TOKEN=$github_token
"$orkia_bin" --repository "$repo" review publish --plan "$plan_id" \
  --github-owner "$owner" --github-repository "$name" --base main --remote origin \
  >"$repo/publish.txt"
pr_url=$(tail -n 1 "$repo/publish.txt")
[[ "$pr_url" == https://github.com/*/pull/* ]]
pr_number=${pr_url##*/}

protection=$(gh api "repos/$full_repo/branches/main/protection")
printf '%s\n' "$protection" | grep -F 'orkia/integrate' >/dev/null
merge_state=$(gh pr view "$pr_number" --repo "$full_repo" --json mergeStateStatus \
  --jq .mergeStateStatus)
test "$merge_state" = "BLOCKED"

echo "GitHub protected-branch E2E proof passed: $pr_url (mergeStateStatus=$merge_state)"
if [[ "${ORKIA_CLEANUP_GITHUB_REPO:-0}" != "1" ]]; then
  echo "Repository retained for inspection: https://github.com/$full_repo" >&2
fi
