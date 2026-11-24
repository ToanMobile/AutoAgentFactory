---
name: crash-triage
description: Root-cause crash, ANR, and native failure triage with paired RED->GREEN regression oracles.
---

# Crash & Production Triage

1. **Gather Evidence Before Editing**
   - Inspect stack traces, failure notes in your prompt, and `traces.txt` / Crashlytics logs (in operator sessions, also check `aaf board trace <task_id>`).
   - Normalize noisy frames (PIDs, timestamps, temp paths) to confirm whether an existing open task or recent 72h closure matches the fingerprint.

2. **Surgical Reproduction & Fix**
   - Identify the exact state invariant, lifecycle race, nullability gap, or main-thread blocking I/O causing the crash.
   - Write a minimal failing test (RED) at the real boundary before modifying production code.
   - Apply the smallest reversible fix so the paired test flips GREEN without loosening existing assertions.

3. **Verification**
   - Confirm no `unwrap()`/`expect()` (Rust) or unchecked `!!` / swallowed exceptions were introduced, run the project's test suite, and in operator sessions re-run `aaf gate`.

## Headless / `AAF_RUN_ID` Mode

When `AAF_RUN_ID` is set, never stop to ask questions or wait for confirmation, and never invoke blocked host commands (`aaf board trace`, `aaf gate`, `aaf approve`). Diagnose from the prompt notes, stack traces, and repository tests directly, land the paired RED -> GREEN regression fix on the task branch, and let the factory run `aaf gate` automatically.
