#!/usr/bin/env bash
# Contract matrix for `orkia init`: fresh repositories, clones, idempotence,
# existing identities, foreign hooks/metadata, corrupt semantic refs, missing
# remotes and permission failures.
set -euo pipefail

workspace_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
orkia_bin=${ORKIA_BIN:-"$workspace_root/target/debug/orkia"}
root=$(mktemp -d "${TMPDIR:-/tmp}/orkia-init-matrix.XXXXXX")

cleanup() {
  chmod -R u+rwx "$root" 2>/dev/null || true
  rm -rf -- "$root"
}
trap cleanup EXIT

cargo build --manifest-path "$workspace_root/Cargo.toml" -p orkia-cli -q

fresh="$root/fresh"
"$orkia_bin" --repository "$fresh" init --create-git --name 'Phase 0 fresh' >/dev/null
first_actor=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$fresh/.git/orkia/actor.json")
first_repo=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])))' "$fresh/.git/orkia/repository.json")
"$orkia_bin" --repository "$fresh" init --name 'Phase 0 fresh' >/dev/null
test "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$fresh/.git/orkia/actor.json")" = "$first_actor"
test "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])))' "$fresh/.git/orkia/repository.json")" = "$first_repo"
if "$orkia_bin" --repository "$fresh" init --name 'Replacement identity' >/dev/null 2>&1; then
  echo 'init unexpectedly replaced an existing identity' >&2
  exit 1
fi

# A foreign hook/metadata entry is not owned by Orkia and survives agent setup.
mkdir "$root/codex-home"
printf '%s\n' '{"hooks":{"Stop":[{"matcher":"","hooks":[{"command":"foreign-tool"}]}]}}' >"$root/codex-home/hooks.json"
CODEX_HOME="$root/codex-home" "$orkia_bin" --repository "$fresh" init --name 'Phase 0 fresh' --agent codex >/dev/null
grep -Fq foreign-tool "$root/codex-home/hooks.json"

# Existing clones and repositories without remotes initialize offline.
git -C "$fresh" config user.name 'Phase 0'
git -C "$fresh" config user.email 'phase0@example.test'
git -C "$fresh" commit --allow-empty -m baseline >/dev/null
clone="$root/clone"
git clone "$fresh" "$clone" >/dev/null 2>&1
"$orkia_bin" --repository "$clone" init --name 'Phase 0 clone' >/dev/null
offline="$root/offline"
git init -b main "$offline" >/dev/null 2>&1
"$orkia_bin" --repository "$offline" init --name 'Phase 0 offline' >/dev/null
test -z "$(git -C "$offline" remote)"

# A malformed semantic state ref must fail closed during init.
git -C "$fresh" update-ref refs/orkia/state/corrupt HEAD
if "$orkia_bin" --repository "$fresh" init --name 'Phase 0 fresh' >/dev/null 2>&1; then
  echo 'init unexpectedly accepted a corrupt semantic ref' >&2
  exit 1
fi
git -C "$fresh" update-ref -d refs/orkia/state/corrupt

# A repository whose Orkia metadata cannot be written must fail explicitly.
readonly_repo="$root/readonly"
git init -b main "$readonly_repo" >/dev/null 2>&1
chmod -R a-w "$readonly_repo/.git"
if "$orkia_bin" --repository "$readonly_repo" init --name 'Phase 0 readonly' >/dev/null 2>&1; then
  echo 'init unexpectedly succeeded without write permission' >&2
  exit 1
fi

echo 'Orkia init contract matrix passed'
