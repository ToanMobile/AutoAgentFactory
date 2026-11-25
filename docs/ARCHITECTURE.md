# Architecture

A map for people changing the code. It describes what exists today and the
rules that keep it maintainable. `docs/PLAN.md` stays the contract for
behaviour; this file is the contract for structure.

## 1. Crates

```
aaf-domain   pure types and port traits, no I/O
   ▲
aaf-fp       tree fingerprint, data-dir layout, HMAC receipts (sync)
   ▲
   ├── aaf-hook     PreToolUse/BeforeTool guard binary (exit 0 allow, 2 deny)
   └── aaf-engine   everything the factory does (gate, runs, storage, UI server)
          ▲
       aaf-cli      the `aaf` binary: argument parsing and output only
```

Arrows point at what a crate may use. `aaf-hook` never depends on
`aaf-engine`: it runs on every tool call of an agent and must stay small,
fast and fail closed. Nothing depends on `aaf-cli`.

## 2. Modules of `aaf-engine`

Grouped by role. A module may use the ones in its own row or below, with the
exceptions listed in section 5.

| Layer | Modules | Role |
|---|---|---|
| Orchestration | `factory`, `plan_debate`, `pr_ops`, `split`, `github_board`, `github_poll`, `watch` | Pick a task, run an agent in a worktree, gate it, audit it, open and merge the PR, move the task |
| Review and reporting | `cross_audit`, `handover`, `proof`, `telemetry`, `web_ui`, `telegram`, `webhook` | Second opinions, acceptance reports, metrics, the local web UI and notifications |
| Execution | `executors`, `roles`, `accounts`, `throttle`, `unblocker`, `gate`, `store_compliance`, `detector` | Agent CLIs, quota, the heavy-command queue, the quality gate |
| Infrastructure | `db`, `git`, `config`, `confine_core`, `hook_settings`, `skills`, `gc*`, `process`, `clock` | SQLite store, git and worktrees, configuration, host confinement/process plumbing, clean-up, processes, time |

`aaf-cli` holds one module per subcommand under `src/cmd/`. A command parses
its flags, calls the engine, and renders one `CmdOutput` (text, or one JSON
object with `--json`; exit 0 ok, 1 error, 2 blocked/policy).

## 3. Life of a task

```
backlog ─► ready ─► running ─► gating ─► review ─► done
                      │          │         │
                      └──────────┴─────────┴──► blocked ─► ready | gating
                    (any state except done) ─► cancelled
```

The legal moves live in `aaf_domain::task::TaskState::can_transition_to`.
Every move goes through `TaskStore::set_state` and is recorded as a
`task_state_changed` event whose payload carries `to` and `why`. That event
stream is the single source of the timing numbers in section 4, so a new code
path that moves a task must emit it too (`factory::Deps::move_to` does both).

One `aaf run` loop: the scheduler picks a ready task, `executors` starts the
agent CLI in a fresh worktree from `git`, `aaf-hook` guards each tool call,
`gate` runs the repo's checks (heavy commands queue in `throttle`),
`cross_audit` asks a second model, the PR is opened and the task moves to
`review`, then `done` after merge. Failures go to `blocked` with a
`BlockedReason`; `unblocker` decides whether to retry, rework or ask a human.

## 4. Observability

| Question | Where to look |
|---|---|
| Everything at once | `aaf ui`, then the Studio link (`http://127.0.0.1:4317/studio`) |
| Tasks done, lead time, time per stage, cost per executor, block reasons | `aaf report [--since HOURS]`, `/api/report` |
| What is running now, accounts, throttle | `aaf status`, `aaf watch`, `/api/status` |
| The full timeline of one task | `aaf board trace <id>`, `aaf handover <id>` |
| Is this machine and repo set up | `aaf doctor`, `/api/doctor`, Studio "Run checks" |

Data lives in `AAF_DATA_DIR` (default `~/.local/share/autofactory`): `aaf.db`
(tables `tasks`, `runs`, `events`, …), signed receipts and approvals, run
logs. Telemetry code only reads; it never writes to the store.

## 5. Dependency rules and the ratchet

`crates/aaf-engine/tests/module_layers.rs` lists every dependency between two
top-level engine modules. The test fails when:

- a new dependency appears: either it belongs there (add it to the list and,
  if it crosses layers, to this section) or the shared code should move down
  a layer;
- a listed dependency disappeared: delete it from the list, so it cannot
  creep back.

It also keeps `clock`, `config`, `confine_core`, `process`, `detector`,
`webhook`, and `telemetry` free of any dependency on the orchestration layer
(`factory`, `plan_debate`, `pr_ops`, `split`, `github_board`, `github_poll`,
`watch`, `web_ui`), and enforces a permanent Tarjan SCC ratchet
(`no_cycles_in_engine_module_graph`) requiring **zero dependency cycles** across
all top-level `aaf-engine` modules.

**Resolved debt (0 cycles).** The legacy 12-module cycle and `handover*`
cross-module knot have been eliminated: shared contracts live in
`aaf_domain::engine_contracts`, low-level host confinement / supervised process
capture / hidden-file inspection / git-ref helpers live in `confine_core`,
locked PR merge + post-merge cleanup orchestration lives in `pr_ops`, and the
`handover` feature is consolidated under `handover/`. Any future PR that
re-introduces a module cycle fails `module_layers::no_cycles_in_engine_module_graph`.

**Remaining acyclic cross-layer edges (ratcheted in `ALLOWED`):**

- `gate -> proof, store_compliance` (quality gate verifies proof receipts and runs store-compliance checks)
- `gc -> gate, telemetry` (garbage collection checks gate receipts/locks and reads telemetry snapshots)
- `hook_settings -> gate` (hook configuration references gate policies)
- `executors -> process` (agent runners manage subprocesses and process groups)
- `telegram -> watch` (Telegram bot queries live watch status)

## 6. House rules

- No `.rs` file over 600 lines, tests included (`aaf-cli/tests/repo_rules.rs`).
  Split by responsibility into a child module (`foo.rs` + `foo/bar.rs`), never
  by loading a sibling file with `#[path]`.
- No `unsafe` outside `executors/sandbox_check.rs`, no `tokio`, no new
  dependency without a note in the PR.
- `unwrap`/`expect` are denied in production code; tests may use them.
- Tests are deterministic: no fixed sleeps, no network, no real `$HOME`
  (use `tempfile` and `AAF_DATA_DIR`), time through the `Clock` port.
- Before pushing: `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`.
