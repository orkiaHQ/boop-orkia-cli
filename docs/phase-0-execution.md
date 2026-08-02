# Phase 0 execution record

Status: in progress. This record is evidence, not a declaration that the
phase is complete.

## Repositories

The three public repositories exist under `orkiaHQ`, use `main`, and have
protected branches with the CI `build` check required:

- `orkiaHQ/boop-orkia-backend` — `https://github.com/orkiaHQ/boop-orkia-backend`
- `orkiaHQ/boop-orkia-frontend` — `https://github.com/orkiaHQ/boop-orkia-frontend`
- `orkiaHQ/boop-orkia-cli` — `https://github.com/orkiaHQ/boop-orkia-cli`

Issues are enabled; Projects and Wiki are disabled. The initial repository
commits and CI runs are retained in GitHub. Bootstrap documentation is being
merged through `boop/e2e/bootstrap-docs` pull requests.

## Local vertical slice

Verified on 2026-08-01:

- PostgreSQL is reachable on `127.0.0.1:5438` and NATS on `127.0.0.1:4223`.
- Backend migrations apply cleanly and the server responds `204` to
  `/health/live` and `/health/ready` on `127.0.0.1:18080`.
- The GraphQL schema is served at `/graphql/schema` and unauthenticated
  requests return the expected `UNAUTHENTICATED` error.
- The frontend builds with `npm ci` and `npm run build`; Vite serves the UI on
  `127.0.0.1:14173` with the backend endpoint configured.
- CLI workspace tests pass (`15` tests) and the real CLI binary is built.

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

## Remaining Phase 0 gates

The authenticated backend ingestion of a signed CLI envelope, cross-repository
ChangeSet submission, GitHub PR projection, and frontend rendering of the
real ChangeSet still need to be exercised. OAuth is currently configured with
local dummy credentials, so a real GitHub login must be wired before that E2E
gate can be marked complete.
