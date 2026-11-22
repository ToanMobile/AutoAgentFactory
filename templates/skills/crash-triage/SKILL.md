---
name: crash-triage
description: Root-cause crash, ANR, and native failure triage with paired RED->GREEN regression oracles.
---

# Crash & Production Triage

1. **Gather Evidence Before Editing**
   - Read execution history via `aaf board trace <task_id>` and inspect stack traces or `traces.txt` / Crashlytics logs.
   - Normalize noisy frames (PIDs, timestamps, temp paths) to confirm whether an existing open task or recent 72h closure matches the fingerprint.

2. **Surgical Reproduction & Fix**
   - Identify the exact state invariant, lifecycle race, nullability gap, or main-thread blocking I/O causing the crash.
   - Write a minimal failing test (RED) at the real boundary before modifying production code.
   - Apply the smallest reversible fix so the paired test flips GREEN without loosening existing assertions.

3. **Verification**
   - Confirm no `unwrap()`/`expect()` (Rust) or unchecked `!!` / swallowed exceptions were introduced, and re-run `aaf gate`.
