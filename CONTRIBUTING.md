# Contributing to Orkia CLI

Contributions target `main` through pull requests. Use branches under
`boop/e2e/` for real-session experiments and never merge an automated E2E
branch into `main`.

Run the checks before submitting a change:

```bash
cargo +1.93.1 fmt --all -- --check
cargo +1.93.1 test --workspace --locked
cargo +1.93.1 clippy --workspace --all-targets --all-features --locked -- -D warnings
```

The ledger and its signed refs are append-only. Do not rewrite `.git/orkia`
history or introduce business decisions into the Git adapter.
