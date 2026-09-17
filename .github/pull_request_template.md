## Summary

<!-- What changed and why (1–3 sentences). -->

## Test plan

- [ ] `cargo fmt --all -- --check` and `cargo clippy --locked --all-targets --all-features -- -D warnings`
- [ ] `cargo test --lib --locked`
- [ ] If you added `tests/<something>_test.rs`: `./scripts/check-integration-tests-in-ci.sh` and a matching `--test` line in `.github/workflows/ci.yml`
- [ ] `cd demo/s3-browser/ui && npm run build && npm run lint && npm run typecheck && npm run knip`
- [ ] **Before merge / release**: `cargo test --all --locked` (MinIO on `localhost:9000`) or rely on nightly `test-all-nightly.yml` after merge
