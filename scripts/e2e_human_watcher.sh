#!/usr/bin/env bash
# Verifies that a human session's persistent workspace watcher records an
# unmediated write and prevents automatic stack generation.
set -euo pipefail

workspace_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
orkia_bin=${ORKIA_BIN:-"$workspace_root/target/debug/orkia"}
repo=$(mktemp -d "${TMPDIR:-/tmp}/orkia-human-watch.XXXXXX")
cleanup() { rm -rf "$repo"; }
trap cleanup EXIT

cargo build --manifest-path "$workspace_root/Cargo.toml" -q -p orkia-cli
git -C "$repo" init -b main >/dev/null
git -C "$repo" config user.name 'Orkia human watcher E2E'
git -C "$repo" config user.email 'orkia-human-watcher@example.test'
printf 'pub fn baseline() {}\n' > "$repo/src.rs"
git -C "$repo" add src.rs
git -C "$repo" commit -m baseline >/dev/null
"$orkia_bin" --repository "$repo" identity init --name 'Orkia human watcher E2E' >/dev/null
"$orkia_bin" --repository "$repo" session start --origin human --objective 'observe editor write' >/dev/null
sleep 0.6
printf 'pub fn unmediated() {}\n' > "$repo/src.rs"
sleep 0.8
output=$("$orkia_bin" --repository "$repo" session checkpoint)
printf '%s\n' "$output" | grep -F 'automatic review withheld'
ledger_json=$(for ref in $(git -C "$repo" for-each-ref --format='%(refname)' refs/orkia/ledger); do
  git -C "$repo" cat-file -p "$(git -C "$repo" rev-parse "$ref")"
done)
printf '%s\n' "$ledger_json" | grep -F '"unknown_write":true'
"$orkia_bin" --repository "$repo" ledger verify >/dev/null
"$orkia_bin" --repository "$repo" session close >/dev/null
echo 'human workspace watcher E2E proof passed'
