# Orkia

Orkia is a self-hosted, Git-native semantic review engine. Git remains the
content and synchronization layer; Orkia records signed causal evidence and
derives deterministic review stacks from it.

## Architecture

The Cargo workspace has deliberately narrow crates. `orkia-model` and
`orkia-ports` are pure domain contracts. Infrastructure adapters depend inward
on those contracts; only the CLI and server assemble concrete implementations.

The canonical ledger is stored in `refs/orkia/ledger`. All indexes are
rebuildable projections.

## Local flow

```sh
cargo build -p orkia-cli
./target/debug/orkia identity init --name "Ada"
./target/debug/orkia session start --objective "Add the parser"
./target/debug/orkia session run -- cargo test --workspace
./target/debug/orkia session checkpoint
./target/debug/orkia ledger verify
./target/debug/orkia review plan
./target/debug/orkia review project --plan <plan-id>
```

For an agent session, Orkia invokes the provider and versions its JSONL output:

```sh
./target/debug/orkia session start --origin codex --objective "Implement parser"
./target/debug/orkia session agent --provider codex -- --json --sandbox read-only "Inspect the task"
```

The agent command deliberately persists a failed invocation too: missing causal
data must reduce confidence rather than silently produce a fragile stack.

## Status

This repository contains the executable v0.1 foundation: signed event model,
Git ledger adapter, capture sessions, semantic atoms, deterministic review
planning, policy checks, GitHub adapter contract, rebuildable index and CLI /
server composition roots.
