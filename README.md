# Orkia

Orkia is a self-hosted, Git-native semantic review engine. Git remains the
content and synchronization layer; Orkia records signed causal evidence and
derives deterministic review StackPullRequests, stacks and ChangeSets from it.

## Vocabulary

A **StackPullRequest** is one small projected PR. A **Stack** is an ordered
set of StackPullRequests in a single repository. A **ChangeSet** coordinates
dependent stacks or PRs across one or more repositories and contains no Git
content itself. This distinction is enforced by the model and the signed Git
refs: `refs/orkia/stack-prs`, `refs/orkia/stacks`, and
`refs/orkia/changesets`.

## Architecture

The Cargo workspace has deliberately narrow crates. `orkia-model` and
`orkia-ports` are pure domain contracts. Infrastructure adapters depend inward
on those contracts; only the CLI and server assemble concrete implementations.

The canonical ledger is stored in `refs/orkia/ledger`. All indexes are
rebuildable projections.

The audited Atomic capability inventory and the evidence-backed Orkia roadmap
are in [docs/atomic-capability-roadmap.md](docs/atomic-capability-roadmap.md).
The current executable evidence and remaining release proofs are tracked in
[docs/changeset-stack-validation.md](docs/changeset-stack-validation.md).

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

On a workstation where the real `codex` binary is authenticated and available,
run `scripts/e2e_codex_local.sh` for the reproducible end-to-end proof. It
creates a fresh Git repository, lets Codex edit it through the native capture
path, verifies the signed ledger, projects a StackPullRequest, proves `main`
is unchanged, and reconstructs the plan from a mirror with only Git refs.

The server webhook boundary is exercised separately by
`scripts/e2e_server_webhook.sh`; it uses a fresh repository, a real HMAC
signature, durable deduplication and `orkia ledger verify`.

For human workspaces, `scripts/e2e_human_watcher.sh` verifies that an editor
write observed outside `orkia run` is signed as `unknown_write` and blocks an
automatic stack.

`scripts/e2e_server_changeset_status.sh` verifies the authenticated HTTP
ChangeSet status endpoint against a fresh Git repository.

With an authenticated `gh` client, `scripts/e2e_github_protected.sh` creates a
temporary public repository, publishes a real PR through `review publish`, and
checks that GitHub blocks its merge until the Orkia check and approval policy
are satisfied. The remote repository is retained for inspection unless
`ORKIA_CLEANUP_GITHUB_REPO=1` is explicitly set.

## Coordinating several repositories

Create a cross-repository ChangeSet only from signed, locally verified Stacks.
It records delivery dependencies (which must already be integrated) but never
copies Git content between the repositories.

```sh
orkia --repository /path/to/coordinator changeset create \
  --stack '<api-repository-id>:<api-stack-id>' \
  --stack '<web-repository-id>:<web-stack-id>' \
  --repository-path '<api-repository-id>=/absolute/path/to/api' \
  --repository-path '<web-repository-id>=/absolute/path/to/web'
```

The command rejects a missing Stack, an unsigned Stack, or a path whose durable
repository identity does not match the declared reference.

Use `orkia changeset status --id <changeset-id> --repository-path …` to obtain
the same Git-ref reconstruction locally. It reports a stack as unpublished
until every exact StackPullRequest revision selected by that Stack has a
signed projection with status `Published`, and exposes the resulting
cross-repository topological `execution_order`.

The integration gate can evaluate the complete ChangeSet, with each repository
keeping its own policy and validation ledger:

```sh
orkia --repository /path/to/coordinator integrate \
  --changeset <changeset-id> --branch main --approvals 2 \
  --repository-path '<api-repository-id>=/absolute/path/to/api' \
  --repository-path '<web-repository-id>=/absolute/path/to/web'
```

It fails closed until all selected projections are forge-published and prints
the exact topological StackPullRequest execution order. GitHub check
publication remains per repository, since a multi-repository ChangeSet does
not collapse independent forge credentials into one Git remote.

The self-hosted server receives its repository registry through
`ORKIA_REPOSITORIES='<repository-id>=/path/to/repo;…'`. Its
`GET /v1/changesets/<id>` endpoint reconstructs the signed ChangeSet and
verifies every referenced Stack in that registry. Its
`ready_for_integration` field is true only when every exact
StackPullRequest revision selected by those Stacks has a signed, published
projection; an absent dependency remains a conflict. Set `ORKIA_POSTGRES_URL` to
rebuild the optional search index from the Git ledgers at server startup; the
endpoint never treats that index as authoritative.
Set `ORKIA_SERVICE_TOKEN` for service-to-service polling; a valid bearer token
authorizes the status endpoints without requiring a human reviewer grant.

GitHub webhook deliveries are HMAC-verified and stored as signed
`forge_webhook` ledger events before acknowledgement. Retries are idempotent
even after a server restart; when more than one repository is registered,
send `x-orkia-repository` to bind the delivery to its ledger.

`docker compose up --build` provides the server and its optional Postgres
projection; the compose file uses the same `ORKIA_POSTGRES_URL` variable as
the binary. Mount repositories and set `ORKIA_REPOSITORIES` when enabling
repository-aware authorization.

An optional, versioned `orkia.toml` configures `protected_branches`,
`validation_commands`, `minimum_coverage_milli`, `minimum_confidence_milli`
and `required_approvals`. `orkia integrate` runs those validations and records
their signed outcomes before permitting integration.

The canonical digest of that policy is embedded in every signed review plan.
If the policy changes, an existing plan cannot be projected or integrated
under the new rules: capture/review must produce a new signed plan first.

## GitHub App

`review publish` and `integrate --github-owner … --github-repository …` use a
GitHub App installation token. For a credential broker, set
`ORKIA_GITHUB_INSTALLATION_TOKEN`. For a normal self-hosted deployment, set
`ORKIA_GITHUB_APP_ID`, `ORKIA_GITHUB_INSTALLATION_ID`, and either
`ORKIA_GITHUB_PRIVATE_KEY_PATH` (recommended) or `ORKIA_GITHUB_PRIVATE_KEY`.
Orkia signs a short-lived RS256 App JWT and exchanges it for an
installation-scoped token; it does not require a personal access token.

When publishing to a protected branch, Orkia configures every policy check and
publishes each required context against every exact projected commit. A failed
policy produces failed checks before the CLI rejects integration.
`review publish` pushes the signed `refs/orkia/*` namespace before it creates
the forge PR, then pushes it again after recording the forge URL. Reviewers can
therefore reconstruct the causal evidence from the same remote as the branch.

## Status

This repository contains the executable v0.1 foundation: signed event model,
Git ledger adapter, capture sessions, semantic atoms, deterministic review
planning, policy checks, GitHub adapter contract, rebuildable index and CLI /
server composition roots.

## Releases

Pushing a `v*` tag invokes `.github/workflows/release.yml`. It builds the CLI
for Linux x86_64 and macOS arm64, publishes SHA-256 checksums plus keyless
Sigstore bundles, and publishes/signs the server image in GHCR. This workflow
requires GitHub Actions OIDC and package permissions; a locally built binary
is not represented as a signed release artifact.
