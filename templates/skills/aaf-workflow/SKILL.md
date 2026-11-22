---
name: aaf-workflow
description: AutoAgentFactory task execution, gating, DAG planning, handover reporting, rework, gc, and telemetry workflow for coding agents and operators.
---

# AutoAgentFactory Workflow

## Inside `aaf run` (Sandboxed Agent)

When `AAF_RUN_ID` is set, you are running inside an isolated worktree and Seatbelt sandbox:
- **Allowed**: read/edit repo source, write paired RED -> GREEN tests (`tdd-workflow`), run project unit/lint commands (wrap heavy builds via `aaf slot -- <cmd>` when needed), and capture device screenshots via `mobile-mcp` (`mobile_save_screenshot`) if configured.
- **Blocked by policy & sandbox**: do **not** run host/operator commands (`aaf gate`, `aaf proof record`, `aaf board rework`, `aaf board unblock`, `aaf board unfreeze`, `aaf board approve-plan`, `aaf skills sync`, `aaf approve`, `aaf gc`, `aaf handover --out/--open`). The data dir and `receipt.key` are intentionally unreachable inside the sandbox; `aaf run` runs the gate, post-gate cross-audit, and PR flow automatically after your turn finishes.
- **Discipline**: never modify `.claude/settings.json`, `.mcp.json`, or head-locked tests unless the task explicitly requires it; keep changes scoped to explicit paths and never use `git add -A`.

## Operator / Interactive Session

Outside `AAF_RUN_ID` (human terminal or orchestrator session):
1. **Inspect, DAG & Plan**:
   - Check `aaf status`, `aaf watch`, `aaf board list`, `aaf board trace <task_id>`, and use `aaf plan "<goal>" [--file ...]` for read-only multi-file design before editing.
   - Manage task dependencies with `aaf board dep add|rm|list` and inspect the dependency graph via `aaf board graph` (cycle-checked DAG).
   - When `[pipeline].plan_stage = true` and `require_plan_approval = true`, review the generated plan and unblock execution with `aaf board approve-plan <task_id>`.
2. **Sync Skills & Doctor**: run `aaf doctor` and `aaf skills sync [--host claude|agents|all]` to verify hooks, sandboxing, and profile skills.
3. **Gate, Proof & Rework**:
   - Capture UI proof via `mobile-mcp` (`mobile_save_screenshot`) and bind it with `aaf proof record|verify`.
   - Run `aaf gate` (`--scoped`, `--full`, or `--release`) to write an HMAC-signed receipt bound to `tree_fp`.
   - Use `aaf approve --reviewer <name>` when head-locked files intentionally changed, and `aaf board rework <task_id> --reason "..." [--rerun]` to return a `review` task to gating or ready.
4. **Handover & Maintenance (v3.2)**:
   - Generate a read-only acceptance handover report with `aaf handover <task_id> [--format md|html|json] [--out <path>] [--open]`. Reports emit `XONG` only when all required gate, proof, and score-gate evidence passes; HTML output enforces a strict CSP and embeds only re-verified PNG proof (`<= 4 MiB`).
   - Clean orphaned batch worktrees, finished task worktrees/branches, snapshot refs, stale fingerprint temp entries, and expired stream/report files via `aaf gc [--dry-run] [--all] [--include-cancelled] [--force] [--older-than-days N]`.
