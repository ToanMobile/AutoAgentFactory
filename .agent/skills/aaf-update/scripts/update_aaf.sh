#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
DEFAULT_AAF_REPO="/Volumes/Data/Toan/AutoAgentFactory"
if [[ -f "$SCRIPT_DIR/../../../../Cargo.toml" ]]; then
  DEFAULT_AAF_REPO="$(cd "$SCRIPT_DIR/../../../.." && pwd -P)"
fi

AAF_REPO="${AAF_REPO:-$DEFAULT_AAF_REPO}"
AAF_BRANCH="${AAF_BRANCH:-main}"
AAF_BUILD_SRC="${AAF_BUILD_SRC:-$HOME/.cache/aaf-update/src}"
AAF_BUILD_TARGET="${AAF_BUILD_TARGET:-$HOME/.cache/aaf-update/target}"
LOCAL_BUILD=0
DO_PULL=0
FORCE_UI_RESTART="${AAF_FORCE_UI_RESTART:-0}"
BUMP_PART=""
RELEASE_PART=""
CHECK_REPOS=()
SYNC_SKILLS_REPOS=()
SYNC_HOOKS_REPOS=()

usage() {
  cat <<'EOF'
Usage: update_aaf.sh [OPTIONS]

Build and install global `aaf` and `aaf-hook` binaries into ~/.cargo/bin so
every repository on this machine immediately uses the latest version.
Each install automatically increments a monotonic build number (`+b<N>.<sha>`)
baked into `aaf --version` without dirtying git.

By default it fetches and builds the newest origin/main (everything merged,
including the embedded web UI), in a separate worktree under
~/.cache/aaf-update, so the local checkout, its branch and its uncommitted
files are never touched.

Options:
  --repo <path>                Override AutoAgentFactory checkout path
  --branch <name>              Build origin/<name> instead of origin/main
  --local                      Build the local checkout as it is (old behaviour)
  --pull                       With --local: run `git pull --ff-only` first
  --bump <patch|minor|major>   With --local: bump [workspace.package] version in Cargo.toml & Cargo.lock (no git commit/push)
  --release <patch|minor|major> Bump version on main, commit, tag vX.Y.Z, push main+tag to GitHub Release, then install locally
  --check-repos <p1> [p2...]   Run read-only `aaf doctor` on listed repos
  --sync-skills <p1> [p2...]   Run `aaf skills sync` on listed repos
  --sync-hooks <p1> [p2...]    Run `aaf init --hook-host all` on listed repos
  --force-restart-ui           Restart Cockpit UI service even if active workers are running
  -h, --help                   Show this help
EOF
}

mode=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --repo)
      AAF_REPO="${2:?missing path for --repo}"
      shift 2
      mode=""
      ;;
    --branch)
      AAF_BRANCH="${2:?missing name for --branch}"
      shift 2
      mode=""
      ;;
    --local)
      LOCAL_BUILD=1
      shift
      mode=""
      ;;
    --pull)
      DO_PULL=1
      shift
      mode=""
      ;;
    --bump)
      BUMP_PART="${2:?missing value for --bump (patch|minor|major)}"
      case "$BUMP_PART" in
        patch|minor|major) ;;
        *)
          echo "error: invalid --bump '$BUMP_PART' (expected patch|minor|major)" >&2
          exit 2
          ;;
      esac
      shift 2
      mode=""
      ;;
    --release)
      RELEASE_PART="${2:?missing value for --release (patch|minor|major)}"
      case "$RELEASE_PART" in
        patch|minor|major) ;;
        *)
          echo "error: invalid --release '$RELEASE_PART' (expected patch|minor|major)" >&2
          exit 2
          ;;
      esac
      shift 2
      mode=""
      ;;
    --check-repos)
      mode="check"
      shift
      ;;
    --sync-skills)
      mode="skills"
      shift
      ;;
    --sync-hooks)
      mode="hooks"
      shift
      ;;
    --force-restart-ui)
      FORCE_UI_RESTART=1
      shift
      mode=""
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --*)
      echo "error: unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
    *)
      case "$mode" in
        check) CHECK_REPOS+=("$1") ;;
        skills) SYNC_SKILLS_REPOS+=("$1") ;;
        hooks) SYNC_HOOKS_REPOS+=("$1") ;;
        *)
          echo "error: unexpected positional argument: $1" >&2
          usage >&2
          exit 2
          ;;
      esac
      shift
      ;;
  esac
done

guard_not_blacklisted() {
  local target="$1"
  if [[ "$target" == *"/agent-workbench"* ]]; then
    echo "error: refusing to touch blacklisted repo: $target" >&2
    exit 2
  fi
}

guard_not_blacklisted "$AAF_REPO"
if [[ ! -f "$AAF_REPO/Cargo.toml" ]]; then
  echo "error: AutoAgentFactory repo not found at $AAF_REPO" >&2
  exit 1
fi

bump_workspace_version() {
  local repo_dir="$1"
  local part="$2"
  python3 - "$repo_dir/Cargo.toml" "$part" <<'PY'
import re, sys
path, part = sys.argv[1], sys.argv[2]
text = open(path, encoding="utf-8").read()
m = re.search(r'(\[workspace\.package\][\s\S]*?^version\s*=\s*")(\d+)\.(\d+)\.(\d+)(")', text, re.M)
if not m:
    sys.exit("error: could not find [workspace.package] version in Cargo.toml")
maj, min_, pat = int(m.group(2)), int(m.group(3)), int(m.group(4))
if part == "major":
    maj, min_, pat = maj + 1, 0, 0
elif part == "minor":
    min_, pat = min_ + 1, 0
else:
    pat += 1
new_v = f"{maj}.{min_}.{pat}"
updated = text[:m.start()] + f"{m.group(1)}{new_v}{m.group(5)}" + text[m.end():]
open(path, "w", encoding="utf-8").write(updated)
print(new_v)
PY
}

if [[ -n "$RELEASE_PART" ]]; then
  if [[ "$LOCAL_BUILD" -eq 1 || -n "$BUMP_PART" || "$AAF_BRANCH" != "main" ]]; then
    echo "error: --release cannot be combined with --local, --bump, or non-main --branch" >&2
    exit 2
  fi
  CUR_BRANCH="$(git -C "$AAF_REPO" branch --show-current)"
  if [[ "$CUR_BRANCH" != "main" ]]; then
    echo "error: --release requires $AAF_REPO to be on branch 'main' (currently '${CUR_BRANCH:-detached}')" >&2
    exit 2
  fi
  if ! git -C "$AAF_REPO" diff --quiet || ! git -C "$AAF_REPO" diff --cached --quiet; then
    echo "error: tracked files in $AAF_REPO have uncommitted changes; commit or stash before --release" >&2
    exit 2
  fi
  echo "==> Syncing main with origin/main..."
  git -C "$AAF_REPO" fetch --tags --quiet origin main
  git -C "$AAF_REPO" pull --ff-only origin main
  echo "==> Running pre-release checks (fmt + repo_rules)..."
  (cd "$AAF_REPO" && cargo fmt --all --check && cargo test -p aaf-cli --test repo_rules --quiet)
  NEW_SEMVER="$(bump_workspace_version "$AAF_REPO" "$RELEASE_PART")"
  NEW_TAG="v${NEW_SEMVER}"
  if git -C "$AAF_REPO" rev-parse -q --verify "refs/tags/${NEW_TAG}" >/dev/null; then
    git -C "$AAF_REPO" checkout -- Cargo.toml
    echo "error: git tag ${NEW_TAG} already exists" >&2
    exit 2
  fi
  echo "==> Bumped [workspace.package] version -> ${NEW_SEMVER} (syncing Cargo.lock)..."
  (cd "$AAF_REPO" && cargo update --workspace --offline >/dev/null)
  # A release ships only a tree with a CLEAR `--release` receipt: the full
  # lint, build and test suite. A receipt already signed for this exact tree
  # is reused, so a re-run after a green gate costs nothing.
  echo "==> Release gate: aaf gate --release (full suite, reused if already CLEAR)..."
  if ! (cd "$AAF_REPO" && aaf gate --release); then
    git -C "$AAF_REPO" checkout -- Cargo.toml Cargo.lock
    echo "error: aaf gate --release is not CLEAR; nothing was committed, tagged or pushed" >&2
    exit 2
  fi
  echo "==> Committing, tagging ${NEW_TAG}, and pushing to GitHub (triggers release.yml)..."
  git -C "$AAF_REPO" add Cargo.toml Cargo.lock
  git -C "$AAF_REPO" commit -m "chore(release): ${NEW_TAG}"
  git -C "$AAF_REPO" tag -a "${NEW_TAG}" -m "Release ${NEW_TAG}"
  git -C "$AAF_REPO" push origin main "${NEW_TAG}"
  echo "    GitHub Release workflow triggered for ${NEW_TAG}: https://github.com/ToanMobile/AutoAgentFactory/releases/tag/${NEW_TAG}"
fi

echo "==> Repo: $AAF_REPO ($(git -C "$AAF_REPO" branch --show-current) @ $(git -C "$AAF_REPO" rev-parse --short HEAD))"
if [[ "$LOCAL_BUILD" -eq 1 ]]; then
  if [[ "$DO_PULL" -eq 1 ]]; then
    echo "==> Pulling latest fast-forward changes..."
    git -C "$AAF_REPO" pull --ff-only
  fi
  SRC="$AAF_REPO"
  echo "==> Building the local checkout as it is (--local)"
else
  if [[ -n "$BUMP_PART" ]]; then
    echo "error: --bump edits Cargo.toml in the checkout; use it together with --local" >&2
    exit 2
  fi
  # Default: always build the newest origin/<branch>, whatever the local
  # checkout is on and whatever is uncommitted there. The build happens in a
  # separate detached worktree, so the working checkout is never touched.
  echo "==> Fetching origin/$AAF_BRANCH..."
  git -C "$AAF_REPO" fetch --quiet origin "$AAF_BRANCH"
  TARGET_REV="$(git -C "$AAF_REPO" rev-parse "origin/$AAF_BRANCH")"
  SRC="$AAF_BUILD_SRC"
  if [[ ! -e "$SRC/.git" ]]; then
    git -C "$AAF_REPO" worktree prune
    mkdir -p "$(dirname "$SRC")"
    git -C "$AAF_REPO" worktree add --detach "$SRC" "$TARGET_REV" >/dev/null
  else
    git -C "$SRC" checkout --quiet --detach --force "$TARGET_REV"
    git -C "$SRC" clean -fdq
  fi
  echo "==> Building origin/$AAF_BRANCH @ $(git -C "$SRC" log -1 --format='%h %s')"
  export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$AAF_BUILD_TARGET}"
fi

PREV_AAF_BIN="$(command -v aaf || echo "$HOME/.cargo/bin/aaf")"
PREV_VER="none"
if [[ -x "$PREV_AAF_BIN" ]]; then
  PREV_VER="$("$PREV_AAF_BIN" --version 2>/dev/null || echo unknown)"
fi

if [[ -n "$BUMP_PART" ]]; then
  NEW_SEMVER="$(bump_workspace_version "$SRC" "$BUMP_PART")"
  echo "==> Bumped [workspace.package] version -> $NEW_SEMVER (syncing Cargo.lock)..."
  (cd "$SRC" && cargo update --workspace --offline >/dev/null)
fi

BUILD_SEQ_FILE="${AAF_DATA_DIR:-$HOME/.local/share/autofactory}/install-build.seq"
mkdir -p "$(dirname "$BUILD_SEQ_FILE")"
PREV_SEQ=0
if [[ -f "$BUILD_SEQ_FILE" ]]; then
  RAW_SEQ="$(tr -d '[:space:]' < "$BUILD_SEQ_FILE" || true)"
  if [[ "$RAW_SEQ" =~ ^[0-9]+$ ]]; then
    PREV_SEQ="$RAW_SEQ"
  fi
fi
NEXT_SEQ=$((PREV_SEQ + 1))

echo "==> Installing aaf & aaf-hook into ~/.cargo/bin (build #${NEXT_SEQ})..."
AAF_BUILD_NUMBER="$NEXT_SEQ" cargo install --path "$SRC/crates/aaf-cli" --locked --force
cargo install --path "$SRC/crates/aaf-hook" --locked --force
printf '%s\n' "$NEXT_SEQ" > "$BUILD_SEQ_FILE"

CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"
AAF_BIN="$CARGO_BIN/aaf"
HOOK_BIN="$CARGO_BIN/aaf-hook"
[[ -x "$AAF_BIN" ]] || AAF_BIN="$(command -v aaf || echo "$HOME/.cargo/bin/aaf")"
[[ -x "$HOOK_BIN" ]] || HOOK_BIN="$(command -v aaf-hook || echo "$HOME/.cargo/bin/aaf-hook")"

# Sign macOS binaries with a stable certificate + fixed identifier so macOS TCC
# keys permissions by Designated Requirement instead of per-build CDHash (prevents
# repeated "aaf would like to access files..." prompts after every rebuild).
# Override via AAF_CODESIGN_IDENTITY=<hash-or-name> or disable with AAF_CODESIGN_IDENTITY=-.
if [[ "$(uname -s)" == "Darwin" && "${AAF_CODESIGN_IDENTITY:-}" != "-" ]] && command -v codesign >/dev/null 2>&1; then
  SIGN_ID="${AAF_CODESIGN_IDENTITY:-}"
  SIGN_LABEL="$SIGN_ID"
  if [[ -z "$SIGN_ID" ]] && command -v security >/dev/null 2>&1; then
    SIGN_LINE="$(security find-identity -v -p codesigning 2>/dev/null | grep -E 'Apple Development:' | head -n 1 || true)"
    if [[ -n "$SIGN_LINE" ]]; then
      SIGN_ID="$(printf '%s\n' "$SIGN_LINE" | awk '{print $2}')"
      SIGN_LABEL="$(printf '%s\n' "$SIGN_LINE" | sed -E 's/.*"([^"]+)".*/\1/')"
    fi
  fi
  if [[ -n "$SIGN_ID" ]]; then
    echo "==> Signing macOS binaries for persistent TCC identity ($SIGN_LABEL)..."
    if codesign --force --sign "$SIGN_ID" --identifier "com.autoagentfactory.aaf" "$AAF_BIN" \
      && codesign --force --sign "$SIGN_ID" --identifier "com.autoagentfactory.aaf-hook" "$HOOK_BIN" \
      && codesign --verify --strict "$AAF_BIN" "$HOOK_BIN" 2>/dev/null; then
      echo "    codesign: OK (com.autoagentfactory.aaf & com.autoagentfactory.aaf-hook)"
    else
      echo "    [WARN] codesign with '$SIGN_LABEL' failed; continuing with linker ad-hoc signature" >&2
    fi
  else
    echo "    [INFO] No Apple Development codesigning identity found; keeping ad-hoc signature (set AAF_CODESIGN_IDENTITY to persist macOS TCC grants)"
  fi
fi

NEW_VER="$("$AAF_BIN" --version)"

echo "==> Installed binaries:"
echo "    previous : $PREV_VER"
echo "    aaf      : $AAF_BIN ($NEW_VER)"
echo "    aaf-hook : $HOOK_BIN"

echo "==> Smoke checking aaf-hook fail-closed guards..."
set +e
printf 'not-json' | "$HOOK_BIN" >/dev/null 2>&1
rc_bad=$?
printf '%s' '{"tool_name":"Bash","tool_input":{"command":"git push --force"}}' | "$HOOK_BIN" >/dev/null 2>&1
rc_force=$?
set -e
if [[ "$rc_bad" -ne 2 || "$rc_force" -ne 2 ]]; then
  echo "error: aaf-hook smoke check failed (bad_json=$rc_bad, force_push=$rc_force; expected 2, 2)" >&2
  exit 1
fi
echo "    aaf-hook smoke: PASS (exit 2 on invalid JSON & git push --force)"

echo "==> Machine check (aaf executors list):"
"$AAF_BIN" executors list

if [[ "$(uname -s)" == "Darwin" && -z "${AAF_RUN_ID:-}" && "${AAF_SKIP_UI_SERVICE:-0}" != "1" ]]; then
  plist_file="$HOME/Library/LaunchAgents/com.autoagentfactory.ui.plist"
  effective_data_dir="${AAF_DATA_DIR:-}"
  if [[ -z "$effective_data_dir" && -f "$plist_file" ]]; then
    effective_data_dir="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:AAF_DATA_DIR" "$plist_file" 2>/dev/null || true)"
  fi
  aaf_db_path="${effective_data_dir:-$HOME/.local/share/autofactory}/aaf.db"
  running_summary=""
  running_check_failed=0
  if [[ -f "$aaf_db_path" ]]; then
    if ! command -v python3 >/dev/null 2>&1; then
      running_check_failed=1
    else
      set +e
      running_summary="$(python3 - "$aaf_db_path" <<'PY' 2>/dev/null
import os, pathlib, sqlite3, subprocess, sys, time
uri = pathlib.Path(sys.argv[1]).resolve().as_uri() + "?mode=ro"
c = sqlite3.connect(uri, uri=True, timeout=5.0)
uid = str(os.getuid())
aaf_run_alive = subprocess.run(
    ["pgrep", "-u", uid, "-f", r"(^|/)aaf([[:space:]]+[^[:space:]]+)*[[:space:]]+run([[:space:]]|$)"],
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
).returncode == 0
rows = list(c.execute("SELECT task_id, started_at FROM runs WHERE status='running' ORDER BY started_at ASC"))
if rows:
    now = int(time.time())
    # Treat rows as active if started within 2h OR an `aaf run` process is alive
    # (prevents ancient stale rows from suppressing UI restarts forever).
    active = [r[0] for r in rows if aaf_run_alive or (r[1] is not None and (now - int(r[1])) < 7200)]
    if active:
        print(f"{len(active)} ({', '.join(active[:6])})")
elif aaf_run_alive:
    # A live `aaf run` process may be between LLM turns (e.g. inside `aaf gate` or merge).
    active_tasks = [r[0] for r in c.execute("SELECT id FROM tasks WHERE state IN ('running','gating') ORDER BY updated_at DESC")]
    if active_tasks:
        print(f"{len(active_tasks)} gating/active ({', '.join(active_tasks[:6])})")
    else:
        print("live aaf run process")
PY
)"
      py_rc=$?
      set -e
      if [[ "$py_rc" -ne 0 ]]; then
        running_check_failed=1
      fi
    fi
  fi
  if [[ "$running_check_failed" -eq 1 && "$FORCE_UI_RESTART" != "1" ]]; then
    echo "==> [WARN] Could not verify active workers in ${aaf_db_path} — binaries updated, skipping Cockpit UI restart to avoid interrupting live runs (pass --force-restart-ui or AAF_FORCE_UI_RESTART=1 to restart anyway)." >&2
  elif [[ -n "$running_summary" && "$FORCE_UI_RESTART" != "1" ]]; then
    echo "==> Active running worker(s): ${running_summary} — binaries updated, skipping Cockpit UI restart to avoid killing in-flight runs (pass --force-restart-ui or AAF_FORCE_UI_RESTART=1 to restart anyway)."
  else
    if [[ -n "$running_summary" ]]; then
      echo "    [WARN] Active running worker(s): ${running_summary} — proceeding with forced UI restart (--force-restart-ui)." >&2
    fi
    ui_help="$("$AAF_BIN" ui --help 2>&1 || true)"
    if [[ "$ui_help" == *"--install-service"* ]]; then
      install_flags=()
      service_env=()
      if [[ "${AAF_UI_ALLOW_ACTIONS:-0}" == "1" ]] || { [[ -f "$plist_file" ]] && grep -q -- '--allow-actions' "$plist_file" 2>/dev/null; }; then
        install_flags+=(--allow-actions)
      fi
      if [[ -f "$plist_file" ]]; then
        for env_key in AAF_WORKTREE_DIR AAF_DATA_DIR AAF_CONFIG_DIR AAF_SEED_CACHE JAVA_HOME ANDROID_HOME ANDROID_SDK_ROOT GRADLE_USER_HOME; do
          if [[ -z "${!env_key:-}" ]]; then
            if saved_val="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:${env_key}" "$plist_file" 2>/dev/null)" && [[ -n "$saved_val" ]]; then
              service_env+=("${env_key}=${saved_val}")
            fi
          fi
        done
      fi
      echo "==> Restarting AAF Cockpit service (com.autoagentfactory.ui ${install_flags[*]:-read-only})..."
      if ! env ${service_env[@]+"${service_env[@]}"} "$AAF_BIN" ui --install-service ${install_flags[@]+"${install_flags[@]}"} >/dev/null; then
        echo "    [WARN] aaf ui --install-service failed (e.g. non-GUI session); skipping UI service restart." >&2
      else
        ui_ok=0
        for _ in 1 2 3 4 5 6 7 8 9 10; do
          if curl -fsS --max-time 1 "http://127.0.0.1:4317/api/health" >/dev/null 2>&1; then
            ui_ok=1
            break
          fi
          sleep 0.3
        done
        if [[ "$ui_ok" -eq 1 ]]; then
          echo "    AAF Cockpit service: UP on http://127.0.0.1:4317 (${install_flags[*]:-read-only}) — Health: http://127.0.0.1:4317/health"
        else
          echo "    [WARN] AAF Cockpit service restarted, but http://127.0.0.1:4317/api/health not ready yet (check ~/Library/Logs/aaf-ui.log)" >&2
        fi
      fi
    elif [[ -f "$plist_file" ]]; then
      echo "==> Restarting existing launchd service com.autoagentfactory.ui..."
      /bin/launchctl kickstart -k "gui/$(id -u)/com.autoagentfactory.ui" || true
    fi
  fi
fi

# Repos `aaf init`-ed before auto-merge / automatic parallelism became the
# default still carry `auto_merge = false` / `max_parallel = 1`. Drop exactly
# those old values (hand-set ones stay) in every repo aaf has tasks for.
echo "==> Migrating old init defaults in known repos (aaf doctor --fix)..."
(cd / && AAF_DOCTOR_FIX_SKIP="/agent-workbench" "$AAF_BIN" doctor --fix | grep ' fix: ') \
  || echo "    nothing to change"

if [[ ${#CHECK_REPOS[@]} -gt 0 ]]; then
  for r in "${CHECK_REPOS[@]}"; do
    guard_not_blacklisted "$r"
    echo "==> Doctor check: $r"
    (cd "$r" && "$AAF_BIN" doctor) || echo "    [WARN] doctor exited non-zero for $r"
  done
fi

if [[ ${#SYNC_SKILLS_REPOS[@]} -gt 0 ]]; then
  for r in "${SYNC_SKILLS_REPOS[@]}"; do
    guard_not_blacklisted "$r"
    echo "==> Syncing skills: $r"
    (cd "$r" && "$AAF_BIN" skills sync)
  done
fi

if [[ ${#SYNC_HOOKS_REPOS[@]} -gt 0 ]]; then
  for r in "${SYNC_HOOKS_REPOS[@]}"; do
    guard_not_blacklisted "$r"
    echo "==> Registering multi-host hooks (claude, codex, gemini): $r"
    (cd "$r" && "$AAF_BIN" init --hook-host all)
    echo "    Note: review git status/diff in $r and commit when ready."
  done
fi

if [[ -x "$AAF_REPO/scripts/link-global-skills.sh" ]]; then
  echo "==> Syncing global AAF skill symlinks..."
  bash "$AAF_REPO/scripts/link-global-skills.sh"
fi

echo "==> Done. All repositories using PATH now run the latest aaf & aaf-hook."
