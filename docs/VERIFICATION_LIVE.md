# AutoAgentFactory — Empirical Live Verification Report

**Date:** `2026-11-23`  
**Harness:** [`scripts/live_verify.sh`](../scripts/live_verify.sh)  
**Scope:** Real macOS Seatbelt (`/usr/bin/sandbox-exec`), real local Android Gradle (`OfficeReader`), real Unity 6000.6.0f1 EditMode (`Idle-Plaza-3D`), live GitHub CLI (`gh`) + `aaf board --dry-run`, and end-to-end `aaf-hook` `PreToolUse` enforcement.

> All target repositories were treated as **read-only** for tracked/untracked files, all `AAF_DATA_DIR` / `AAF_CONFIG_DIR` state was isolated under `/tmp`, and all GitHub checks used strictly read-only queries and `--dry-run`.

---

## 1. Environment & Tool Versions

| Component | Observed Value |
| :--- | :--- |
| **OS** | macOS `27.2.1` (Build `26B131`) |
| **Architecture / CPU** | `arm64` — Apple M4 Pro |
| **Rust / Cargo** | `rustc 1.100.0 (0ef1bc386 2026-11-09)` / `cargo 1.100.0 (3acc79c0c 2026-11-05)` |
| **Git** | `git version 2.56.0` |
| **GitHub CLI (`gh`)** | `gh version 2.103.3 (2026-10-26)` — account `ToanMobile` (scopes: `admin:public_key`, `gist`, `project`, `read:org`, `repo`; token masked) |
| **Android Gradle / JDK** | Gradle `9.8.0`, Kotlin `2.4.10`, Launcher JVM `17.0.21.0.3` (`JAVA_HOME=/opt/homebrew/opt/openjdk@17/...`), Daemon JVM Java 21 (JetBrains), `ANDROID_HOME=/Volumes/Data/AndroidSDK` |
| **Unity Editor** | `6000.6.0f1 (f7f8ed4d1e24)` at `/Applications/Unity/Hub/Editor/6000.6.0f1/Unity.app/Contents/MacOS/Unity` |
| **AAF Binaries** | `aaf 0.1.0` (`$HOME/.cargo/bin/aaf`), `$HOME/.cargo/bin/aaf-hook` |
| **Seatbelt Binary** | `/usr/bin/sandbox-exec` |

---

## 2. Empirical Summary Matrix

| Check | Target | Mode | Result | Duration | Notes |
| :--- | :--- | :--- | :---: | ---: | :--- |
| **1. Seatbelt Isolation Core** | Throwaway `/tmp` worktree + fake `AAF_DATA_DIR` | `sandbox-exec` | **PASS** | 37ms (4 probes, ~9ms each) | `receipt.key` read → `EPERM`; outside write → `EPERM`; worktree write & `/dev/null` → OK |
| **2a. `aaf doctor --json`** | `OfficeReader` | Read-only (isolated `/tmp` data dir) | **PASS** | 88ms | All required checks `ok: true`; advisory notices: `gradle_daemon`, `gradle_daemon_config`, `keychain_sandbox_exposure` |
| **2b. Gradle `--version` via `aaf slot`** | `OfficeReader` (repo read-only) | Exact `aaf` Seatbelt + `aaf slot` | **PASS** | 2.11s | Gradle `9.8.0` / Kotlin `2.4.10` succeeded (`rc=0`); `~/.gradle/init.d` write denied (`EPERM`) |
| **2c. Gradle Unit Test (`:core:common:testDebugUnitTest`)** | `OfficeReader` (disposable `/tmp` git worktree) | Exact `aaf` Seatbelt + `aaf slot` (`--no-daemon --max-workers=2`) | **PASS** | 189.76s | `BUILD SUCCESSFUL` (`119` actionable tasks: `42` executed, `77` from cache), `rc=0`, zero Seatbelt errors |
| **3a. `aaf doctor --json`** | `Idle-Plaza-3D` | Read-only (isolated `/tmp` data dir) | **PASS** | 68ms | All required checks `ok: true`, `10/10` Unity skills synced |
| **3b. Unity `-version`** | Unity `6000.6.0f1` | Exact `aaf` Seatbelt | **PASS** | 0.40s | Printed `6000.6.0f1` (`rc=0`) |
| **3c. Unity EditMode (current `aaf` Seatbelt)** | `Idle-Plaza-3D` on `/Volumes/Data/...` | Current `sandbox.rs` / `mod.rs` profile | **FAIL** | 2.68s | **Gap discovered:** aborts with `Project folder or disk is read only` (`rc=1`) because `/Volumes/Data/.TemporaryItems` is denied |
| **3d. Unity EditMode (+ `.TemporaryItems/folders.<uid>`)** | `Idle-Plaza-3D` on `/Volumes/Data/...` | `aaf` Seatbelt + `/Volumes/Data/.TemporaryItems` | **PASS** | 42.50s | **13/13** `Idle.Tests.IdleSimTests.*` passed (`0.22s` test time, `rc=0`); narrowest fix verified with `folders.501` (`6.22s` startup check) |
| **4a. Live `gh` auth + repo/issue read** | `ToanMobile/AutoAgentFactory` | Read-only live `gh` | **PASS** | 1.72s | `ToanMobile` authenticated with `repo` & `project` scopes; `gh repo view` & `gh issue list` `rc=0` |
| **4b. `aaf board import-issues --dry-run`** | Disposable `/tmp` repo → `ToanMobile/AutoAgentFactory` | Live `gh`, `--dry-run`, isolated `/tmp` DB | **PASS** | 619ms | Empty allowlist → fail-closed `rc=2` (`status: "refused"`, 6ms); allowlisted `ToanMobile` → `rc=0`, `dry_run: true`, `fetched: 0` |
| **4c. `aaf board sync-github --dry-run`** | Disposable `/tmp` repo + 1 local task | `--dry-run`, isolated `/tmp` DB | **PASS** | 13ms | `rc=0`, mapped `ready` → column `"Ready"`, `mode: "project_v2"`, `errors: 0` |
| **5. `aaf-hook` `PreToolUse` Smoke Suite** | 7 JSON payloads (2 allow, 5 deny) | Live `$HOME/.cargo/bin/aaf-hook` | **PASS** | 3.3ms–9.3ms / call | 7/7 exact exit codes (`0` allow, `2` deny) with structured JSON `permissionDecision: "deny"` |

---

## 3. Exact Seatbelt Findings & Recommended Patches

### Finding 1 (Blocking Bug): Unity on macOS secondary volumes (`/Volumes/<Vol>/...`) fails `read-only` check without `<volume>/.TemporaryItems`

- **Observed failure with current `crates/aaf-engine/src/executors/{mod,sandbox}.rs`:**
  Running Unity `6000.6.0f1` `-batchmode -projectPath /Volumes/Data/<workspace>/Game/Idle-Plaza-3D -runTests -testPlatform EditMode` under the exact SBPL profile generated by `aaf` aborts in `2.68s` (`0.25s` on warm licensing IPC) with exit code `1`:
  ```text
  Successfully changed project path to: /Volumes/Data/<workspace>/Game/Idle-Plaza-3D
  Canceling DisplayDialog: Project folder or disk is read only Please move the project folder somewhere readable and writable.
  This should not be called when not controlled by a human.
  [ExitDontLaunchBugReporter] Exiting without the bug reporter. Application will exit with return code 1
  ```
- **Empirical A/B root-cause isolation:**
  - Allowing `~/Library/Preferences`, `~`, `/Library`, `/private/var`, or `(subpath "/Volumes/Data/<workspace>/Game")` all **still failed** with `Project folder or disk is read only`.
  - Allowing `(literal "/Volumes/Data")` or `(regex #"^/Volumes/Data/[^/]+$")` **still failed**.
  - Allowing `(subpath "/Volumes/Data/.TemporaryItems")` **immediately fixed it** (`rc=0`, `read_only_err=False`), even with `sandbox.rs`'s exact `(deny file-write* (literal "<worktree>"))` and `(deny file-write* (literal "<worktree>/.git"))` guards active.
  - Narrowing further to the per-UID directory `(subpath "/Volumes/Data/.TemporaryItems/folders.501")` **also passed** (`rc=0`, `6.22s`).
  - With `/Volumes/Data/.TemporaryItems` opened, the full Unity EditMode suite ran inside `sandbox-exec` + `aaf slot` and passed **13/13** tests in `42.50s` (`rc=0`).
- **Why this happens:** On macOS APFS secondary/external volumes (`/Volumes/<Name>/...`), Foundation/Carbon (`NSFileManager` / `NSItemReplacementDirectory`) resolves volume-local temporary storage to `<VolumeMountPoint>/.TemporaryItems/folders.<uid>`. `ExecEnv::from_process()` currently opens only `/tmp`, `/var/tmp`, `$TMPDIR`, and the sibling `C` cache directory on the boot volume.
- **Recommended fix in `crates/aaf-engine/src/executors/mod.rs` (or `sandbox.rs`):**
  When the canonical `worktree` path starts with `/Volumes/<VolName>/`, add `/Volumes/<VolName>/.TemporaryItems/folders.<uid>` (or `/Volumes/<VolName>/.TemporaryItems` if the per-UID subdirectory does not yet exist) to `temp_dirs` / writable paths for non-read-only runs. This keeps the rest of `/Volumes/<VolName>` completely write-denied while unblocking Unity (and any Cocoa/Foundation atomic-save tooling) on secondary volumes.

### Finding 2 (Non-Fatal): Unity Test Runner secondary write to `Application.persistentDataPath`

- During the passing 13/13 EditMode run (with `-testResults /tmp/.../editmode.xml`), Unity logged one non-fatal caught exception:
  ```text
  UnauthorizedAccessException: Access to the path "$HOME/Library/Application Support/DefaultCompany/Idle-Plaza-3D/TestResults.xml" is denied.
  ```
- **Impact:** None (`rc=0`, explicit `-testResults` XML was written cleanly and all 13 test cases passed). Keeping `~/Library/Application Support/<Company>/<Product>` closed in `BUILD_CACHE_DIRS` remains the safer default.

### Finding 3 (Operational / Diagnostic): `aaf slot` guest detection depends on pre-existing `throttle/drain.lock`

- `crates/aaf-engine/src/throttle/paths.rs::is_guest()` probes `<data_dir>/throttle/drain.lock` with `OpenOptions::write(true)` and treats `PermissionDenied` as confined guest mode.
- Inside `aaf run`, `open_queue()` calls `slots::prepare_for_guests()`, which creates `drain.lock` beforehand, so confined agents work seamlessly.
- However, if an operator or verification script invokes `sandbox-exec` + `aaf slot` directly against a fresh `AAF_DATA_DIR` where `drain.lock` has not been created yet, `open(drain.lock)` returns `NotFound` instead of `PermissionDenied`, `is_guest()` returns `false`, and `locks_dir()` fails with `EPERM` on `create_dir(data_dir)`. Furthermore, `crates/aaf-cli/src/cmd/slot.rs:162` maps any `PermissionDenied` from `throttle.run_slot()` to exit `126` with `"cannot start the command: Operation not permitted (os error 1)"`, obscuring that the throttle lock setup (not the child command) failed.
- **Recommendation:** Either distinguish throttle-setup `PermissionDenied` from child-spawn `PermissionDenied` in `slot.rs`, or have `is_guest()` treat `throttle/` read-only + `throttle/guest/` writable as guest mode even before `drain.lock` exists.

### Finding 4 (Confirmed Good): Android Gradle 9.8.0 + Kotlin 2.4.10 + KSP + Hilt inside Seatbelt

- Both `./gradlew --no-daemon --version` (`2.11s`, repo read-only) and a full cold-worktree `:core:common:testDebugUnitTest --no-daemon --max-workers=2` (`189.76s`, `119` actionable tasks: `42` executed, `77` from cache) succeeded with `rc=0` under `aaf`'s **unmodified** Seatbelt rules (`BUILD_CACHE_DIRS`, `CACHE_WRITE_DENIED`, `SDK_WRITE_DENIED`, guarded literal roots, `AAF_DATA_DIR` denied with `throttle/` + `throttle/guest/` reopened).
- Writing to `~/.gradle/init.d/evil.gradle` inside the same sandbox returned `Operation not permitted` (`rc=1`), confirming Gradle cache-poisoning defenses hold during real builds.

---

## 4. Detailed Experiment Logs

### Experiment 1 — Seatbelt Isolation Core

| Probe | Command inside `sandbox-exec` | Exit Code | Latency | Observed Result |
| :--- | :--- | :---: | ---: | :--- |
| 1a | `cat <AAF_DATA_DIR>/receipt.key` | `1` | 9.1ms | `Operation not permitted` |
| 1b | `echo pwn > <outside>/escape.txt` | `1` | 10.4ms | `Operation not permitted` (`exists=False`) |
| 1c | `echo ok > <worktree>/allowed.txt` | `0` | 8.9ms | Created with content `'ok'` |
| 1d | `echo hi > /dev/null` | `0` | 9.0ms | Succeeded |

### Experiment 4 — Live GitHub CLI & `aaf board --dry-run`

- `gh auth status`: `rc=0` (`516ms`), account `ToanMobile`, active, SSH protocol, scopes `'admin:public_key', 'gist', 'project', 'read:org', 'repo'`.
- `gh repo view ToanMobile/AutoAgentFactory --json nameWithOwner,isPrivate,defaultBranchRef`: `rc=0` (`554ms`), `{"defaultBranchRef":{"name":"main"},"isPrivate":false,"nameWithOwner":"ToanMobile/AutoAgentFactory"}`.
- `gh issue list --repo ToanMobile/AutoAgentFactory --limit 5 --json number,title,state`: `rc=0` (`646ms`), `[]`.
- `aaf --json board import-issues --dry-run` (empty `owner_github_allowlist`): `rc=2` (`6ms`), `{"command":"board","duration_ms":6,"ok":false,"status":"refused"}`.
- `aaf --json board import-issues --dry-run` (`owner_github_allowlist = ["ToanMobile"]`): `rc=0` (`619ms`), `{"command":"board","created":[],"dry_run":true,"duration_ms":613,"fetched":0,"ok":true,"skipped_open":[],"status":"ok","suppressed_recent":[]}`.
- `aaf --json board sync-github --dry-run` (1 local task `t-76da3d04` in `ready` state): `rc=0` (`13ms`), `{"command":"board","dry_run":true,"duration_ms":8,"errors":0,"items":[{"column":"Ready","issue_number":null,"state":"ready","synced":false,"task_id":"t-76da3d04","unchanged":false}],"mode":"project_v2","ok":true,"skipped_no_issue":["t-76da3d04"],"status":"ok","truncated_project_items":false,"unchanged":0}`.

### Experiment 5 — `aaf-hook` `PreToolUse` End-to-End Smoke Check

| Payload (`Bash.command`) | Expected | Actual | Latency | Guard Triggered |
| :--- | :---: | :---: | ---: | :--- |
| `git status` | `0` | `0` | 5.7ms | *(allowed — `{}`)* |
| `aaf slot -- ./gradlew --version` | `0` | `0` | 3.8ms | *(allowed — `{}`)* |
| `git push --force origin main` | `2` | `2` | 3.6ms | `git_guard` |
| `rm .env` | `2` | `2` | 9.3ms | `delete_guard` |
| `gh pr merge 42 --squash` | `2` | `2` | 3.6ms | `pr_merge_guard` |
| `./gradlew build` | `2` | `2` | 3.5ms | `heavy_cmd_guard` |
| `aaf approve` | `2` | `2` | 3.3ms | `human_action_guard` |

---

## 5. Repository Cleanliness Verification

Both real target repositories were verified before and after all runs:

- `/Volumes/Data/<workspace>/OfficeReader/OfficeReader`: `git status --short` was empty before and after; disposable Gradle worktree removed and pruned via `git worktree remove --force` + `git worktree prune`.
- `/Volumes/Data/<workspace>/Game/Idle-Plaza-3D`: `git status --short` was empty before and after; Unity `-testResults` XML and `-logFile` were written exclusively inside `/tmp` and cleaned up on exit.

---

## 6. Re-Running Verification

```bash
# Full non-destructive verification across Android, Unity, GitHub (--dry-run), and aaf-hook:
scripts/live_verify.sh \
  --android /Volumes/Data/<workspace>/OfficeReader/OfficeReader \
  --unity /Volumes/Data/<workspace>/Game/Idle-Plaza-3D \
  --github-repo ToanMobile/AutoAgentFactory

# Optional: also compile & run a real Gradle unit-test task in a disposable git worktree inside Seatbelt:
scripts/live_verify.sh \
  --android /Volumes/Data/<workspace>/OfficeReader/OfficeReader \
  --android-unit-test :core:common:testDebugUnitTest \
  --skip-unity

# Fast CI/local smoke check without Android/Unity repos:
scripts/live_verify.sh --skip-android --skip-unity
```
