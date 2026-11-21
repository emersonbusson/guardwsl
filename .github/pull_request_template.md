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
- [ ] Linux process-ownership tests with read-only sudo
- [ ] `python3 -m unittest discover -s tests -p 'test_*.py'`
- [ ] `bash tests/install_platform.sh`
- [ ] `bash tests/install_order.sh`
- [ ] `bash tests/retire_shims.sh`
- [ ] `cargo audit --deny warnings`
- [ ] `cargo deny --locked check`
- [ ] Shell syntax checked for each installer script
- [ ] Documentation updated when behavior changed

## Related issue

Closes #
