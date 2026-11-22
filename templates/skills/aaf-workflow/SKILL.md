---
name: aaf-workflow
description: AutoAgentFactory task execution, gating, rework, and telemetry workflow for coding agents and operators.
---

# AutoAgentFactory Workflow

## Inside `aaf run` (Sandboxed Agent)

When `AAF_RUN_ID` is set, you are running inside an isolated worktree and Seatbelt sandbox:
- **Allowed**: read/edit repo source, write paired RED -> GREEN tests (`tdd-workflow`), run project unit/lint commands (wrap heavy builds via `aaf slot -- <cmd>` when needed), and capture device screenshots via `mobile-mcp` (`mobile_save_screenshot`) if configured.
- **Blocked by policy & sandbox**: do **not** run host/operator commands (`aaf gate`, `aaf proof record`, `aaf board rework`, `aaf board unblock`, `aaf board unfreeze`, `aaf skills sync`, `aaf approve`). The data dir and `receipt.key` are intentionally unreachable inside the sandbox; `aaf run` runs the gate and PR flow automatically after your turn finishes.
- **Discipline**: never modify `.claude/settings.json`, `.mcp.json`, or head-locked tests unless the task explicitly requires it; keep changes scoped to explicit paths and never use `git add -A`.

## Operator / Interactive Session

Outside `AAF_RUN_ID` (human terminal or orchestrator session):
1. **Inspect & Plan**: check `aaf status`, `aaf board list`, `aaf board trace <task_id>`, and use `aaf plan "<goal>" [--file ...]` for read-only multi-file design before editing.
2. **Sync Skills & Doctor**: run `aaf doctor` and `aaf skills sync [--host claude|agents|all]` to verify hooks, sandboxing, and profile skills.
3. **Gate, Proof & Rework**:
   - Capture UI proof via `mobile-mcp` (`mobile_save_screenshot`) and bind it with `aaf proof record|verify`.
   - Run `aaf gate` (`--scoped`, `--full`, or `--release`) to write an HMAC-signed receipt bound to `tree_fp`.
   - Use `aaf approve --reviewer <name>` when head-locked files intentionally changed, and `aaf board rework <task_id> --reason "..." [--rerun]` to return a `review` task to gating or ready.
