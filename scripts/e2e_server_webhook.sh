#!/usr/bin/env bash
# Verifies the authenticated GitHub webhook path against a real local Git
# repository and the compiled Orkia server. No mock ledger is involved.
set -euo pipefail

workspace_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
server_bin=${ORKIA_SERVER_BIN:-"$workspace_root/target/debug/orkia-server"}
orkia_bin=${ORKIA_BIN:-"$workspace_root/target/debug/orkia"}
repo=$(mktemp -d "${TMPDIR:-/tmp}/orkia-webhook-repo.XXXXXX")
log=$(mktemp "${TMPDIR:-/tmp}/orkia-webhook-server.XXXXXX.log")
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
git -C "$repo" config user.name 'Orkia webhook E2E'
git -C "$repo" config user.email 'orkia-webhook-e2e@example.test'
printf 'baseline\n' > "$repo/README.md"
git -C "$repo" add README.md
git -C "$repo" commit -m baseline >/dev/null
"$orkia_bin" --repository "$repo" identity init --name 'Orkia webhook E2E' >/dev/null
repository_id=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])))' "$repo/.git/orkia/repository.json")

start_server() {
  ORKIA_BIND="127.0.0.1:$port" \
  ORKIA_REPOSITORIES="$repository_id=$repo" \
  ORKIA_GITHUB_WEBHOOK_SECRET=local-secret \
  "$server_bin" >"$log" 2>&1 &
  server_pid=$!
  for _ in $(seq 1 80); do
    if curl -fsS "http://127.0.0.1:$port/health" >/dev/null 2>&1; then break; fi
    sleep 0.1
  done
}
start_server
curl -fsS "http://127.0.0.1:$port/health" >/dev/null

payload='{"action":"opened","repository":{"full_name":"orkia/webhook-e2e"}}'
signature=$(PAYLOAD="$payload" python3 - <<'PY'
import hashlib, hmac, os
print("sha256=" + hmac.new(b"local-secret", os.environ["PAYLOAD"].encode(), hashlib.sha256).hexdigest())
PY
)
status=$(curl -sS -o /dev/null -w '%{http_code}' -X POST \
  -H "x-hub-signature-256: $signature" \
  -H 'x-github-event: pull_request' \
  -H 'x-github-delivery: webhook-e2e-1' \
  -H 'content-type: application/json' \
  --data "$payload" "http://127.0.0.1:$port/webhooks/github")
test "$status" = 202
duplicate=$(curl -sS -o /dev/null -w '%{http_code}' -X POST \
  -H "x-hub-signature-256: $signature" \
  -H 'x-github-event: pull_request' \
  -H 'x-github-delivery: webhook-e2e-1' \
  -H 'content-type: application/json' \
  --data "$payload" "http://127.0.0.1:$port/webhooks/github")
test "$duplicate" = 204
kill "$server_pid"
wait "$server_pid" 2>/dev/null || true
start_server
durable_duplicate=$(curl -sS -o /dev/null -w '%{http_code}' -X POST \
  -H "x-hub-signature-256: $signature" \
  -H 'x-github-event: pull_request' \
  -H 'x-github-delivery: webhook-e2e-1' \
  -H 'content-type: application/json' \
  --data "$payload" "http://127.0.0.1:$port/webhooks/github")
test "$durable_duplicate" = 204
"$orkia_bin" --repository "$repo" ledger verify >/dev/null
echo "server webhook E2E proof passed: signed delivery $repository_id"
