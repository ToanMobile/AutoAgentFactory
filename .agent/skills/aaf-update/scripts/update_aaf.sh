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
  ui_help="$("$AAF_BIN" ui --help 2>&1 || true)"
  if [[ "$ui_help" == *"--install-service"* ]]; then
    install_flags=()
    plist_file="$HOME/Library/LaunchAgents/com.autoagentfactory.ui.plist"
    if [[ "${AAF_UI_ALLOW_ACTIONS:-0}" == "1" ]] || { [[ -f "$plist_file" ]] && grep -q -- '--allow-actions' "$plist_file" 2>/dev/null; }; then
      install_flags+=(--allow-actions)
    fi
    echo "==> Restarting AAF Cockpit service (com.autoagentfactory.ui ${install_flags[*]:-read-only})..."
    if ! "$AAF_BIN" ui --install-service "${install_flags[@]}" >/dev/null; then
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
        echo "    AAF Cockpit service: UP on http://127.0.0.1:4317 (${install_flags[*]:-read-only}) — Studio: http://127.0.0.1:4317/studio"
      else
        echo "    [WARN] AAF Cockpit service restarted, but http://127.0.0.1:4317/api/health not ready yet (check ~/Library/Logs/aaf-ui.log)" >&2
      fi
    fi
  elif [[ -f "$HOME/Library/LaunchAgents/com.autoagentfactory.ui.plist" ]]; then
    echo "==> Restarting existing launchd service com.autoagentfactory.ui..."
    /bin/launchctl kickstart -k "gui/$(id -u)/com.autoagentfactory.ui" || true
  fi
fi

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
