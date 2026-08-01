# Architecture rules

`orkia-model` owns data and errors. `orkia-ports` owns interfaces. Neither may
depend on infrastructure, a forge, a database, the filesystem, or Git.

Infrastructure crates (`identity`, `ledger`, `git`, `semantic`, `capture`,
`policy`, `forge`, `github`, and `index-postgres`) depend inward only. They do
not import another infrastructure implementation. `orkia-cli` and
`orkia-server` are the only composition roots.

The signed ledger in `refs/orkia/ledger` is authoritative. SQLite/Postgres and
forge state are disposable projections. Reviewer corrections supersede a review
plan; they never alter captured causal events.

Every new adapter must first implement a port contract and its contract tests.
