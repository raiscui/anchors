Before handing off a change:
1. `cargo fmt` plus `cargo clippy --all-targets --all-features` to ensure style + lint compliance.
2. `cargo test --all-features` (and targeted `cargo test --package anchors --lib ...` for touched modules) to confirm incremental graph logic stays intact.
3. For perf-sensitive work, run the relevant `cargo bench --bench ...` microbench.
4. Update docs (`readme.md`, `changelog.md`) when APIs or behavior change.
5. Append entries to WORKLOG.md (summary of edits) and ERRORFIX.md (root cause + fix for any error) per project rule.