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
   - Any changes to locked agent config or workflow definitions require explicit review via `aaf approve`.
