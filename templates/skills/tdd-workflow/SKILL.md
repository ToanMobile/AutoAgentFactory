---
name: tdd-workflow
description: Paired RED->GREEN regression oracle discipline, surviving-mutant guard, anti-tautology checks, and safe test authoring.
---

# TDD Workflow & Paired Oracle Discipline

1. **Paired RED -> GREEN Oracle**
   - Before editing production code for any bug fix or testable behavior change, write or select a failing test at the real failure boundary and observe it fail (**RED**) for the expected reason.
   - Apply the minimal production change, then run the exact same test and verify it passes (**GREEN**).
   - **Never revert or delete user work to fabricate a RED run**; if the tree already contains an in-progress fix, verify the failure via an isolated regression test targeting the uncovered edge case or a clean boundary unit test.

2. **Anti-Tautology & Surviving-Mutant Guard**
   - Every test must assert observable behavior or domain invariants, never mock return values back onto themselves or mirror production formulas verbatim.
   - Mentally mutate the fix (flip a condition, drop a guard, return early): if the test would still pass, strengthen the assertion before declaring completion.
   - Do not weaken, delete, or skip existing assertions to make a suite green; changes to head-locked existing tests require explicit review (`aaf approve`).

3. **Scoped & Honest Verification**
   - Run the smallest targeted test command first during iteration, then finish with the project's scoped/full verification suite without relying on cached or stale results.

## Headless / `AAF_RUN_ID` Mode

When `AAF_RUN_ID` is set, never stop to ask questions or wait for confirmation, and never invoke blocked host commands (`aaf gate`, `aaf approve`). Run the project's own test runner directly for the RED -> GREEN cycle, keep head-locked existing tests untouched unless required by the task spec, and finish cleanly on the task branch so the factory executes `aaf gate` and cross-audit automatically.
