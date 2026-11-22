---
name: merge-conflict-resolver
description: Safe Git merge, rebase, and cherry-pick conflict resolution preserving both intents with post-resolution verification.
---

# Merge Conflict Resolver

1. **Understand Both Sides Before Editing**
   - Inspect conflicted paths (`git status --short`, `git diff --check`) and read both ours (`HEAD`) and theirs (`MERGE_HEAD` / incoming commit) surrounding context before choosing lines.
   - Preserve the behavioral intent of **both** branches unless one side explicitly supersedes the other in the task spec; never blindly take `ours` or `theirs` on shared logic, migrations, or config.

2. **Clean Resolution Hygiene**
   - Remove every conflict marker (`<<<<<<<`, `=======`, `>>>>>>>`, `|||||||`) and verify `git diff --check` reports zero stray markers or trailing whitespace artifacts.
   - Never hand-edit generated lockfiles or build artifacts into invalid syntax; regenerate them cleanly or resolve according to the package manager's rules.
   - Respect head-locked files (`.autofactory/config.toml`, `.claude/**`, `.agents/**`, `.mcp.json`, existing tests): if a conflict touches them, flag the change clearly for review.

3. **Post-Resolution Verification**
   - Re-run scoped unit/integration tests covering both merged behaviors after resolving conflicts.
   - In operator sessions, re-run `aaf gate` on the resolved tree before merging or pushing; never reuse a pre-conflict receipt.
