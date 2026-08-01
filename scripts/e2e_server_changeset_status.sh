#!/usr/bin/env bash
# Exercises server ChangeSet reconstruction over HTTP with a real repository.
set -euo pipefail

workspace_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
server_bin=${ORKIA_SERVER_BIN:-"$workspace_root/target/debug/orkia-server"}
orkia_bin=${ORKIA_BIN:-"$workspace_root/target/debug/orkia"}
repo=$(mktemp -d "${TMPDIR:-/tmp}/orkia-server-status.XXXXXX")
log=$(mktemp "${TMPDIR:-/tmp}/orkia-server-status.XXXXXX.log")
port=$(python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)
cleanup() {
  if [[ -n "${server_pid:-}" ]]; then kill "$server_pid" 2>/dev/null || true; fi
  rm -rf "$repo" "$log"
}
trap cleanup EXIT

cargo build --manifest-path "$workspace_root/Cargo.toml" -q -p orkia-cli -p orkia-server
git -C "$repo" init -b main >/dev/null
git -C "$repo" config user.name 'Orkia server status E2E'
git -C "$repo" config user.email 'orkia-server-status@example.test'
printf 'pub fn baseline() {}\n' > "$repo/src.rs"
git -C "$repo" add src.rs
git -C "$repo" commit -m baseline >/dev/null
"$orkia_bin" --repository "$repo" identity init --name 'Orkia server status E2E' >/dev/null
repository_id=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])))' "$repo/.git/orkia/repository.json")
"$orkia_bin" --repository "$repo" session start --origin codex --objective 'server status proof' >/dev/null
"$orkia_bin" --repository "$repo" session run -- sh -c "printf 'pub fn status_feature() {}\\n' > src.rs" >/dev/null
"$orkia_bin" --repository "$repo" session checkpoint >/dev/null
plan_id=$(git -C "$repo" for-each-ref --format='%(refname)' refs/orkia/plans | sed -E 's#refs/orkia/plans/([^/]+)/.*#\1#' | sort -u | head -n 1)
test -n "$plan_id"
"$orkia_bin" --repository "$repo" review project --plan "$plan_id" >/dev/null
stack_id=$(git -C "$repo" for-each-ref --format='%(refname)' refs/orkia/stacks | sed -E 's#refs/orkia/stacks/([^/]+)/.*#\1#' | sort -u | head -n 1)
test -n "$stack_id"
"$orkia_bin" --repository "$repo" changeset create --stack "$repository_id:$stack_id" --repository-path "$repository_id=$repo" >/dev/null
changeset_id=$(git -C "$repo" for-each-ref --format='%(refname)' refs/orkia/changesets | sed -E 's#refs/orkia/changesets/([^/]+)/.*#\1#' | sort -u | head -n 1)
test -n "$changeset_id"

ORKIA_BIND="127.0.0.1:$port" \
ORKIA_SERVICE_TOKEN=server-status-secret \
ORKIA_REPOSITORIES="$repository_id=$repo" \
"$server_bin" >"$log" 2>&1 &
server_pid=$!
for _ in $(seq 1 80); do
  if curl -fsS "http://127.0.0.1:$port/health" >/dev/null 2>&1; then break; fi
  sleep 0.1
done
status=$(curl -fsS -o "$repo/status.json" -w '%{http_code}' -H 'authorization: Bearer server-status-secret' "http://127.0.0.1:$port/v1/changesets/$changeset_id")
test "$status" = 200
grep -F '"ready_for_integration":false' "$repo/status.json" >/dev/null
grep -F '"execution_order"' "$repo/status.json" >/dev/null
"$orkia_bin" --repository "$repo" ledger verify >/dev/null
echo "server ChangeSet status E2E proof passed: $changeset_id"
