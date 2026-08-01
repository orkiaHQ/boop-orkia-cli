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

The Git-native Atomic capability-parity roadmap is in
[docs/atomic-capability-roadmap.md](docs/atomic-capability-roadmap.md).

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
./target/debug/orkia integrate --plan <plan-id> --approvals 1
```

For an agent session, Orkia invokes the provider and versions its JSONL output:

```sh
./target/debug/orkia session start --origin codex --objective "Implement parser"
./target/debug/orkia session agent --provider codex -- --json --sandbox read-only "Inspect the task"
```

The agent command deliberately persists a failed invocation too: missing causal
data must reduce confidence rather than silently produce a fragile stack.

An optional, versioned `orkia.toml` configures `protected_branches`,
`validation_commands`, `minimum_coverage_milli`, `minimum_confidence_milli`
and `required_approvals`. `orkia integrate` runs those validations and records
their signed outcomes before permitting integration.

Captured commits can be inspected without a separate database: `orkia semantic
diff --base <commit>`, `semantic blame --path <path>` and `semantic log` read
only verified Git-backed manifests and label uncaptured history as Git
fallback. Signed access grants may be scoped to repository IDs, teams and an
RFC 3339 expiry; team-backed grants require a signed organization/team chain
trusted by `authorized_grant_issuers` in the versioned policy.

## Status

This repository contains the executable v0.1 foundation: signed event model,
Git ledger adapter, capture sessions, semantic atoms, deterministic review
planning, policy checks, GitHub adapter contract, rebuildable index and CLI /
server composition roots.
