---
name: security-gate
description: Trust-boundary, sandboxing, allowlist, path safety, and secret-hygiene checklist for AAF and target apps.
---

# Security Gate Checklist

1. **Untrusted Inputs & External Triggers**
   - Fail closed on GitHub issues (`safety.owner_github_allowlist`), webhook tokens, intents, deep links, and external files.
   - Frame untrusted issue/body text clearly before feeding it into agent prompts.

2. **Filesystem & Process Safety**
   - Canonicalize paths and block `..`, symlinks (`symlink_metadata`), and arbitrary host path reads.
   - Keep private keys, receipts, and SQLite DBs owner-only (`0700` dirs, `0600` files); never commit `.env`, `.keystore`, or credentials.
   - Resolve `git`/`gh` executables safely; execute subprocesses via explicit argv vectors without shell interpolation.

3. **Approval & Head-Lock**
   - Any changes to locked agent config or workflow definitions require explicit operator review via `aaf approve` (outside `AAF_RUN_ID`).

## Headless / `AAF_RUN_ID` Mode

When `AAF_RUN_ID` is set, never stop to ask questions or wait for confirmation, and never invoke blocked host commands (`aaf approve`, `aaf gate`). Enforce every trust-boundary, path-safety, and secret-hygiene requirement directly in code and tests, avoid editing head-locked files unless strictly required by the task spec, and let the factory's automated gate and cross-audit verify the change.
