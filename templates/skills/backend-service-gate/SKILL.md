---
name: backend-service-gate
description: Backend/Rust service reliability, bounded timeouts, schema migrations, resource cleanup, and structured PII-free logging.
---

# Backend & Systems Service Gate

1. **Correctness & Complexity Boundaries**
   - Avoid O(N^2) lookups where maps/sets fit, close streams/files/DB handles reliably, and keep error propagation explicit without silent swallowing or panicking unwraps in production paths.

2. **Network, Timeouts & Database Migrations**
   - Enforce bounded connect/read/command timeouts, retry only idempotent operations with jittered backoff, and guard mutating endpoints with idempotency keys.
   - Pair every schema change with a reversible migration and migration test; never emit destructive schema drops against live data.

3. **Observability & Secrets**
   - Use structured logging with secrets, tokens, emails, and PII masked; verify unit and integration coverage cleanly (and via `aaf gate` in operator sessions).

## Headless / `AAF_RUN_ID` Mode

When `AAF_RUN_ID` is set, never stop to ask questions or wait for confirmation, and never invoke blocked host commands (`aaf gate`, `aaf approve`). Run the repository's own unit, integration, and lint commands directly (wrapping heavy builds via `aaf slot -- <cmd>`) and commit cleanly on the task branch so the factory executes `aaf gate` and cross-audit automatically.
