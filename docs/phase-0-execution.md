# Phase 0 execution record

Status: implementation complete for the local vertical slice. GitHub OAuth and
real PR publication remain external acceptance gates.

## Repositories

The three public repositories exist under `orkiaHQ`, use `main`, and have
protected branches with their repository CI checks required:

- `orkiaHQ/boop-orkia-backend` — `https://github.com/orkiaHQ/boop-orkia-backend`
- `orkiaHQ/boop-orkia-frontend` — `https://github.com/orkiaHQ/boop-orkia-frontend`
- `orkiaHQ/boop-orkia-cli` — `https://github.com/orkiaHQ/boop-orkia-cli`

Issues are enabled; Projects and Wiki are disabled. The initial repository
commits and CI runs are retained in GitHub. Bootstrap documentation was merged
through `boop/e2e/bootstrap-docs` pull requests: backend `384a7d2`, frontend
`a591d4b`, and CLI `05d35d8`.

## Local vertical slice

Verified on 2026-08-01:

- PostgreSQL is reachable on `127.0.0.1:5438` and NATS on `127.0.0.1:4223`.
- Backend migrations apply cleanly and the server responds `204` to
  `/health/live` and `/health/ready` on `127.0.0.1:18080`.
- The GraphQL schema is served at `/graphql/schema` and unauthenticated
  requests return the expected `UNAUTHENTICATED` error.
- The frontend builds with `npm ci` and `npm run build`; Vite serves the UI on
  `127.0.0.1:14173` with the backend endpoint configured.
- The frontend Project Operations view now queries and renders the backend's
  repository ChangeSet detection projection (`repositoryChangeSets`). The
  view labels it as evidence and does not present it as the canonical signed
  multi-repository ChangeSet; the UI change is merged in frontend `6e85d84`.
- The backend now stores verified signed ChangeSet envelopes in the immutable
  `canonical_changesets` table, keyed by `(changeset_id, revision)` and
  deduplicated by payload hash. CLI checkpoints and automatic agent Stop hooks
  submit the exact Ed25519-covered payload when `ORKIA_BACKEND_URL` is set.
- The frontend now queries `repositoryCanonicalChangeSets` and renders a
  separate ledger-backed canonical ChangeSet card; detector output remains
  explicitly labeled as an evidence projection.
- `orkia init --create-git` creates a repository when needed, writes and
  validates a default `orkia.toml`, and reports the policy/ref/backend wiring.
- CLI workspace tests pass (`15` tests) and the real CLI binary is built.
- `scripts/e2e_server_changeset_status.sh` passes with a local service token:
  the HTTP server reconstructs a signed ChangeSet from Git refs and returns
  its execution order with `ready_for_integration: false` until projection.

## Real Codex capture

A fresh clone of `boop-orkia-backend` was initialized with `orkia init`, then a
real `codex exec` session created `docs/codex-e2e-proof.md` and ran
`git diff --check` without a manual `orkia session start`.

Evidence from that clone:

- `orkia ledger verify`: `verified 22 signed ledger events`;
- automatic SessionStart, prompt, tool, file-write, snapshot and Stop events;
- `orkia review plan`: one atom, one review unit, coverage `1000‰`;
- signed `refs/orkia/plans/*`, `refs/orkia/stacks/*` and
  `refs/orkia/stack-prs/*` were created;
- the absolute file path emitted by Codex is correlated with Git's relative
  path by the regression-tested path normalizer.

The same no-manual-session scenario was also run in fresh initialized clones
of the frontend and CLI repositories. Their evidence is:

- frontend: `orkia ledger verify` reported `19` signed events and
  `orkia review plan` reported one atom, one unit, coverage `1000‰`;
- CLI: `orkia ledger verify` reported `19` signed events and
  `orkia review plan` reported one atom, one unit, coverage `1000‰`.

## Remaining Phase 0 gates

The signed CLI envelope path is exercised against the local backend and is
idempotent (`idempotent: true` on replay, one database row). Three seeded local
fixtures mapped to the real public GitHub repositories
`boop-orkia-backend`, `boop-orkia-frontend`, and `boop-orkia-cli` each produced
an automatic signed plan, stack, and ChangeSet; their canonical rows are
visible in Postgres and via `/api/v1/changesets/{id}`.

The remaining acceptance gates require a real authenticated browser session:
GitHub OAuth, projection of those ChangeSets into real GitHub PRs/checks, and a
browser assertion of the canonical frontend card. Local credentials are
configured for the server, but no browser login was performed in this run.
