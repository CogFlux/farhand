# FarHand — instructions for coding agents

Read `README.md` first; it is the contract. Code, comments, docs and commit
messages are in English.

## Gates

Run all four before every commit:

```
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo check --all-targets
cargo test --all
```

## Invariants

- Nothing crosses to the remote without `farhand_core::guard::Guard`
  seeing it first: command text, file content, edits, every uploaded file.
- The local filesystem is reached only through `farhand_core::local::LocalScope`.
- The local environment is never forwarded to the remote.
- Every tool call writes one audit record, including refusals. File contents
  and guard-matched text are never logged.
- `farhand-core` has no MCP dependency; `farhand` has no SSH dependency.

## Git

Stage files by name. Never commit `target/`.
