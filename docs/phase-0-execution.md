# Phase 0 execution record

Status: Phase 0 acceptance scenario verified on 2026-08-01. The Ghost PR
benchmark, production GitHub App deployment, Claude E2E, Sigstore publication
and premium PR Shape remain later v0.1 gates.

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
  `/health/live` and `/health/ready` on `127.0.0.1:8080`; the worker process is
  running alongside the server.
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
- The Phase 0 operations view is data-driven: it displays the actual signed
  proof count, signer, stack/dependency counts and backend validation state;
  unavailable runtime projections are labeled as unavailable rather than
  filled with sample records (frontend commit `d7a164a`).
- `orkia init --create-git` creates a repository when needed, writes and
  validates a default `orkia.toml`, and reports the policy/ref/backend wiring.
- `orkia init` publishes the Ed25519 actor certificate at
  `refs/orkia/actors/<actor-id>` and verifies the complete ledger before it
  reports success. This makes a fresh clone independently verifiable rather
  than trusting a local key file.
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

## Automatic cross-repository publication

The reference scenario used three registered fresh clones and the same Codex
prompt in each repository. No `orkia session start`, `changeset create`,
`review project` or `review publish` command was issued. Stop hooks derived the
plan, stack, projection, GitHub PR and ChangeSet automatically:

- ChangeSet `999f94b1-6885-52a5-b9a0-de715426d297` contains exactly three stacks;
- the backend API returned revision `0`, status `active`, and a payload with
  exactly three `stacks` and three signed `proofs` with service authentication;
- backend PR [#4](https://github.com/orkiaHQ/boop-orkia-backend/pull/4), frontend
  PR [#6](https://github.com/orkiaHQ/boop-orkia-frontend/pull/6) and CLI PR
  [#7](https://github.com/orkiaHQ/boop-orkia-cli/pull/7) were created by the
  automatic publication path and all required CI jobs passed;
- the frontend data-driven operations hardening is isolated in PR
  [#7](https://github.com/orkiaHQ/boop-orkia-frontend/pull/7) (commit
  `d7a164a`) and the clone-verification hardening is isolated in CLI PR
  [#8](https://github.com/orkiaHQ/boop-orkia-cli/pull/8); both have passing CI
  and remain open for the required human review on protected `main`;
- publication authentication used the configured GitHub installation token;
  the local OAuth dashboard flow was also exercised against the running stack.
- The backend registry is bound to the real `orkiaHQ` namespace and GitHub
  repository IDs (`1319687696`, `1319687694`, `1319687699`), not the temporary
  `riftrHQ` seed namespace.

The exact proof is retained in `docs/phase0-cross-repo-e2e.md`. GitHub rejects
arbitrary custom refs under `refs/orkia/*`; Orkia transports immutable signed
objects through `refs/tags/orkia-meta/*` and recreates canonical local refs on
fetch. This remains ordinary Git object/ref transport.

## Clone reconstruction

Three new clones (`*-reconstruct`) ran `orkia init` and
`orkia ledger fetch --remote origin`. Each clone then independently ran
`orkia ledger verify` successfully (`42`, `37` and `37` signed events); no
source working tree or private signing key was copied. The coordinator then reconstructed
ChangeSet `999f94b1-6885-52a5-b9a0-de715426d297`: `orkia changeset show` returned
three stacks and `orkia changeset status` over the three clones returned
`ready_for_integration: true`, with all three stack PRs published and a
deterministic execution order.

The generated stack PRs are intentionally not merged by this scenario;
protected `main` still requires the configured human approval and Orkia
integration check.

The Git remote round-trip test covers this transport, including blob/tree
semantic objects transported as lightweight tags.

## Explicitly out of Phase 0

Ghost PR causal thresholds (gain ≥20%, separated pairs <10%, ARI ≥0.8), a
production GitHub App RS256/check-run run, Claude Code success, Sigstore
signing and release binaries are later v0.1 gates documented in
`orkia-unification-implementation-plan.md`.
