---
name: qa-review
description: Pre-PR diff audit, acceptance-criteria traceability, edge-case probing, blast-radius check, and secret/placeholder screening.
---

# Pre-PR QA Review

1. **Acceptance Criteria & Diff Completeness**
   - Map every acceptance criterion in the task spec to concrete code paths and executable tests in the diff; flag any unverified requirement explicitly.
   - Keep the diff surgical: reject drive-by formatting sweeps, unrelated refactors, placeholder stubs (`TODO`, `unimplemented!()`, `// ... existing code ...`), or dead code left behind.

2. **Edge Cases & Blast Radius**
   - Trace all callers and consumers of changed signatures, config schemas, or persisted formats (SQLite, JSON, TOML) to ensure backward compatibility and safe migrations.
   - Probe boundary inputs: empty/missing values, zero/max integers (`saturating_*`), non-UTF8 or symlinked paths, concurrency/locking races, and partial failure cleanup.

3. **Safety, Secrets & Proof Integrity**
   - Screen the diff for hardcoded secrets, tokens, `.env`/keystore files, PII in logs, `unwrap()`/`expect()` in production Rust, or silenced errors.
   - Verify UI or mobile changes carry fresh, verified visual proof matching the final tree before opening or merging a PR.
