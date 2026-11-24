---
name: qa-review
description: AutoAgentFactory automated Pre-PR diff audit, 7 Golden Standards (/100) + MechanicalScore evaluation, acceptance-criteria traceability, edge-case probing, and secret screening (overrides interactive QA skills when AAF_RUN_ID is set).
---

# AutoAgentFactory Pre-PR QA Review

## Headless / `AAF_RUN_ID` Mode (Overrides Interactive QA Flows)

When `AAF_RUN_ID` is set, you are running headlessly with no human present. This automated audit discipline **overrides** any user-level interactive Q&A or acceptance-matrix `qa-review` skill:
- **NEVER** stop to ask clarifying questions, **NEVER** request human confirmation, and **NEVER** invoke blocked host commands (`aaf gate`, `aaf approve`, `aaf proof record`).
- Audit your own diff autonomously against every acceptance criterion below, fix any gap directly in code and tests, run the project's test/lint suite, and commit cleanly on the task branch; the factory runs `aaf gate` and independent cross-audit automatically.

1. **Acceptance Criteria & Diff Completeness**
   - Map every acceptance criterion in the task spec to concrete code paths and executable tests in the diff; flag any unverified requirement explicitly.
   - Keep the diff surgical: reject drive-by formatting sweeps, unrelated refactors, placeholder stubs (unfinished `TODO` notes, panic-stub macros, or ellipsis markers omitting code), or dead code left behind.

2. **Edge Cases & Blast Radius**
   - Trace all callers and consumers of changed signatures, config schemas, or persisted formats (SQLite, JSON, TOML) to ensure backward compatibility and safe migrations.
   - Probe boundary inputs: empty/missing values, zero/max integers (`saturating_*`), non-UTF8 or symlinked paths, concurrency/locking races, and partial failure cleanup.

3. **Safety, Secrets & Proof Integrity**
   - Screen the diff for hardcoded secrets, tokens, `.env`/keystore files, PII in logs, `unwrap()`/`expect()` in production Rust, or silenced errors.
   - Verify UI or mobile changes carry fresh, verified visual proof matching the final tree before opening or merging a PR.

4. **7 Golden Standards (`G1..G7`, `/100`), `MechanicalScore` & Evidence Tags (v3.2)**
   - Score every criterion `G1..G7` out of `100` and state a concrete deduction reason whenever a score is `< 100`:
     - `G1`: Correctness & Edge Cases
     - `G2`: Architecture & Modularity
     - `G3`: Error Handling & Observability
     - `G4`: Security & Trust Boundaries
     - `G5`: Performance & Resource Hygiene
     - `G6`: Tests & Verification Quality
     - `G7`: Readability, Diff Scope & Maintainability
   - **`MechanicalScore` (`/100`)**: computed deterministically from the signed gate receipt, static/store findings, proof verification, and reviewer independence (`same_account`). Effective score is `min(MechanicalScore, LLM average)`. When `[cross_audit].score_gate = true`, passing requires `effective_score >= min_average_score` (default `95`) and every `G1..G7 >= min_criterion_score` (default `90`).
   - **Evidence Tagging**: tag every tool-executed gate/test/static/proof/mechanical metric with `[tự chạy]` and every LLM-judged score or qualitative assessment with `[agent báo]`. Never omit gaps or present `[agent báo]` claims as `[tự chạy]`.
