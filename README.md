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

## Status

This repository contains the executable v0.1 foundation: signed event model,
Git ledger adapter, capture sessions, semantic atoms, deterministic review
planning, policy checks, GitHub adapter contract, rebuildable index and CLI /
server composition roots.
