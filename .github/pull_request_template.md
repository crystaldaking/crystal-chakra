## Summary

Describe the user-visible or architectural outcome.

## Validation

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --locked --workspace --all-targets -- -D warnings`
- [ ] `cargo test --locked --workspace`
- [ ] Rust fixture, rustdoc, and `cargo deny check`
- [ ] Relevant real-provider or performance checks, when applicable
- [ ] `git diff --check`

## Review

- [ ] Precision/provenance and revision/freshness claims are honest
- [ ] Ordinary edits do not trigger full repository parsing
- [ ] MCP/LSP/Git protocol types remain inside adapters
- [ ] Documentation and ADRs match the implementation
- [ ] This targets `develop`, or is an explicit `release/*` / `hotfix/*` flow
