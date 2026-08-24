## Summary

Describe the problem and the smallest change that solves it.

## Safety impact

- [ ] No cleanup behavior changed.
- [ ] Cleanup behavior changed and every new candidate remains exact-allowlist,
      fail-closed, and covered by refusal-path tests.
- [ ] Host behavior remains read-only and bounded.

Explain any checked item that is not self-evident.

## Verification

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --locked --all-targets -- -D warnings`
- [ ] `cargo test --locked`
- [ ] `cargo audit --deny warnings`
- [ ] `cargo deny check`
- [ ] `bash -n scripts/install-linux.sh scripts/install-shims.sh`
- [ ] Documentation updated when behavior changed

## Related issue

Closes #
