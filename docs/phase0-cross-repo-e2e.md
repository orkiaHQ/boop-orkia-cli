# Phase 0 cross-repository E2E evidence

This is the reproducible record for the 2026-08-01 Phase 0 run. The three
repositories are real GitHub repositories and the capture sessions were real
Codex sessions.

| Repository | registered ID | automatic PR |
| --- | --- | --- |
| `orkiaHQ/boop-orkia-backend` | `11111111-1111-4111-8111-111111111111` | [#4](https://github.com/orkiaHQ/boop-orkia-backend/pull/4) |
| `orkiaHQ/boop-orkia-frontend` | `22222222-2222-4222-8222-222222222222` | [#6](https://github.com/orkiaHQ/boop-orkia-frontend/pull/6) |
| `orkiaHQ/boop-orkia-cli` | `33333333-3333-4333-8333-333333333333` | [#7](https://github.com/orkiaHQ/boop-orkia-cli/pull/7) |

The same prompt in each clone asked Codex to create a documentation file and
run `git diff --check`. The author did not invoke a session, plan, stack,
ChangeSet or PR command.

## Automatic results

ChangeSet `999f94b1-6885-52a5-b9a0-de715426d297` contains:

- backend stack `c5d428c3-007e-526f-9963-333e1b4799d8`;
- frontend stack `c7a2ec57-38f5-569a-9083-33a8c0ff5376`;
- CLI stack `4d12025f-a89d-5af5-a71a-155e0dd83bcb`.

The authenticated backend query was:

```sh
curl -H 'authorization: Bearer phase0-service' \
  http://localhost:8080/api/v1/changesets/999f94b1-6885-52a5-b9a0-de715426d297
```

It returned revision `0`, status `active`, and a payload containing exactly
three `stacks` and three signed `proofs`. GitHub checks for PRs #4, #6 and #7
passed. The local backend registry uses the real `orkiaHQ` namespace and
GitHub repository IDs, so the authenticated UI renders the same organization
and repository names rather than a fixture namespace.

## Reconstruction proof

Fresh clones in `e2e/boop/*-reconstruct` ran `orkia init` and
`orkia ledger fetch --remote origin`. `orkia init` fetched the public actor
certificates from `refs/tags/orkia-meta/actors/*`, recreated the local
`refs/orkia/actors/*` registry, and `orkia ledger verify` independently
validated 42, 37 and 37 signed events. The coordinator then ran:

```sh
orkia changeset show --id 999f94b1-6885-52a5-b9a0-de715426d297
orkia changeset status --id 999f94b1-6885-52a5-b9a0-de715426d297 \
  --repository-path 11111111-1111-4111-8111-111111111111=<backend-clone> \
  --repository-path 22222222-2222-4222-8222-222222222222=<frontend-clone> \
  --repository-path 33333333-3333-4333-8333-333333333333=<cli-clone>
```

The result was the same three-stack ChangeSet with
`ready_for_integration: true`, three published stack PRs and a deterministic
topological execution order.

GitHub rejects custom top-level `refs/orkia/*` updates. Orkia maps immutable
refs to `refs/tags/orkia-meta/*`, downloads ordinary Git objects and recreates
`refs/orkia/*` locally. The `orkia-git` round-trip test covers commit-like and
blob/tree objects.
