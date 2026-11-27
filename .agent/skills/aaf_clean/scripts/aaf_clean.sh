#!/usr/bin/env bash
set -euo pipefail
shopt -s nullglob

DRY_RUN=0
INCLUDE_CACHES=0
TARGET_REPO=""
MIN_AGE_MINS=120

canonicalize_dir() {
  local p="$1"
  if [[ -d "$p" ]]; then
    (cd "$p" >/dev/null 2>&1 && pwd -P)
  else
    printf '%s\n' "${p%/}"
  fi
}

BLACKLIST_REPO=$(canonicalize_dir "/Volumes/Data/Toan/agent-workbench")
AAF_WT_DIR="/Volumes/Data/Toan/aaf-wt"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --include-caches)
      INCLUDE_CACHES=1
      shift
      ;;
    --repo)
      if [[ -z "${2:-}" ]]; then
        echo "Error: --repo requires a path argument" >&2
        exit 1
      fi
      if [[ ! -d "$2" ]]; then
        echo "Error: --repo path does not exist: $2" >&2
        exit 1
      fi
      TARGET_REPO=$(canonicalize_dir "$2")
      shift 2
      ;;
    --min-age-mins)
      if [[ -z "${2:-}" || ! "${2:-}" =~ ^[0-9]+$ ]]; then
        echo "Error: --min-age-mins requires a non-negative integer" >&2
        exit 1
      fi
      MIN_AGE_MINS="$2"
      shift 2
      ;;
    -h|--help)
      cat <<'EOF'
Usage: aaf_clean.sh [OPTIONS]
  --dry-run            Preview changes without modifying disk or git metadata
  --repo <PATH>        Clean only a specific git repository (skips global aaf-wt & system caches)
  --include-caches     Also clean Gradle build-cache/daemon, Xcode DerivedData, npm/uv caches (when idle)
  --min-age-mins <N>   Minimum idle age (minutes) before removing worktrees or aaf-wt/* (default: 120)
EOF
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      exit 1
      ;;
  esac
done

log() { printf '[aaf_clean] %s\n' "$*"; }

is_blacklisted() {
  local p
  p=$(canonicalize_dir "$1")
  [[ "$p" == "$BLACKLIST_REPO" || "$p" == "$BLACKLIST_REPO/"* ]]
}

# Fast, boundary-safe check whether any active build/agent process holds files inside a directory
dir_in_use() {
  local dir="$1"
  local base
  base=$(basename "$dir")

  # Match full path or path-bounded basename in process args (avoid matching unrelated short substrings)
  if pgrep -f "$dir(/| |$)|/$base(/| |$)" >/dev/null 2>&1; then
    return 0
  fi

  # Check open file descriptors / cwd across common dev/agent/shell processes with exact path boundary
  local lsof_out=""
  lsof_out=$(lsof -c cargo -c rustc -c aaf -c java -c gradle -c claude -c codex -c node -c npm -c python3 -c xcodebuild -c swift -c git -c bash -c zsh -Fn 2>/dev/null || true)
  if [[ -n "$lsof_out" ]] && printf '%s\n' "$lsof_out" | grep -Eq "^n${dir}(/|$)"; then
    return 0
  fi
  return 1
}

# Fail-closed check: returns 0 if any file inside dir was modified within MIN_AGE_MINS or if find errors
dir_recently_modified() {
  local dir="$1"
  if [[ "$MIN_AGE_MINS" -eq 0 ]]; then
    return 1
  fi
  local hit=""
  hit=$(find "$dir" -mmin -"$MIN_AGE_MINS" -print -quit 2>/dev/null || echo "FIND_ERR")
  if [[ -n "$hit" ]]; then
    return 0
  fi
  return 1
}

resolve_base_branch() {
  local repo="$1"
  for candidate in main master develop; do
    if git -C "$repo" show-ref --verify --quiet "refs/heads/$candidate"; then
      echo "$candidate"
      return 0
    fi
  done
  # Fallback to origin/HEAD target if local branch exists
  local origin_head=""
  origin_head=$(git -C "$repo" symbolic-ref --short refs/remotes/origin/HEAD 2>/dev/null || true)
  if [[ -n "$origin_head" ]]; then
    local short_name="${origin_head#origin/}"
    if git -C "$repo" show-ref --verify --quiet "refs/heads/$short_name"; then
      echo "$short_name"
      return 0
    fi
  fi
  return 1
}

# Fail-closed check: returns 0 ONLY if worktree is clean, unlocked, idle, and all commits are merged into base_branch
worktree_safe_to_remove() {
  local repo="$1"
  local wt
  wt=$(canonicalize_dir "$2")
  local base_branch="$3"

  # 1. Check if worktree is locked via porcelain output (fail-closed if git command fails)
  local wt_list=""
  if ! wt_list=$(git -C "$repo" worktree list --porcelain 2>/dev/null); then
    log "  SKIP (git worktree list failed): $wt"
    return 1
  fi
  if printf '%s\n' "$wt_list" | awk -v target="$wt" '
    BEGIN { found=0; is_locked=0 }
    $0 == "worktree " target { found=1; next }
    /^worktree / { found=0 }
    found && /^locked/ { is_locked=1; exit 0 }
    END { exit (is_locked ? 0 : 1) }
  '; then
    log "  SKIP (locked): $wt"
    return 1
  fi

  # 2. Check if any running process is using this worktree
  if dir_in_use "$wt"; then
    log "  SKIP (in use by process): $wt"
    return 1
  fi

  # 3. Check if recently modified
  if dir_recently_modified "$wt"; then
    log "  SKIP (recently active < ${MIN_AGE_MINS}m): $wt"
    return 1
  fi

  # 4. Check dirty status (fail-closed if git status fails)
  local status_out=""
  if ! status_out=$(git -C "$wt" status --porcelain 2>/dev/null); then
    log "  SKIP (git status failed): $wt"
    return 1
  fi
  if [[ -n "$status_out" ]]; then
    return 1
  fi

  # 5. Check unmerged commits (fail-closed: must be ancestor, OR no merge commits and 0 cherry '+' commits)
  local head_sha=""
  if ! head_sha=$(git -C "$wt" rev-parse --verify HEAD 2>/dev/null); then
    log "  SKIP (cannot resolve HEAD): $wt"
    return 1
  fi
  if git -C "$repo" merge-base --is-ancestor "$head_sha" "$base_branch" 2>/dev/null; then
    return 0
  fi

  # git cherry ignores merge commits; refuse cherry fallback if any merge commit exists in base..HEAD
  local merge_hits=""
  if ! merge_hits=$(git -C "$repo" rev-list --merges -n 1 "${base_branch}..${head_sha}" 2>/dev/null); then
    return 1
  fi
  if [[ -n "$merge_hits" ]]; then
    return 1
  fi

  local cherry_out=""
  if ! cherry_out=$(git -C "$repo" cherry "$base_branch" "$head_sha" 2>/dev/null); then
    return 1
  fi
  if printf '%s\n' "$cherry_out" | grep -q '^+'; then
    return 1
  fi
  return 0
}

get_disk_free() {
  local target_vol="/Volumes/Data"
  [[ -d "$target_vol" ]] || target_vol="/System/Volumes/Data"
  [[ -d "$target_vol" ]] || target_vol="/"
  df -h "$target_vol" 2>/dev/null | awk 'NR==2 {print $4}' || echo "unknown"
}

if [[ -n "$TARGET_REPO" ]] && is_blacklisted "$TARGET_REPO"; then
  log "ERROR: Refusing to run on blacklisted repository: $TARGET_REPO"
  exit 1
fi

free_before=$(get_disk_free)
log "Disk free before: $free_before (dry_run=$DRY_RUN)"

# 1. Run built-in aaf gc
if command -v aaf >/dev/null 2>&1; then
  if [[ -n "$TARGET_REPO" ]]; then
    log "Step 1: Running aaf gc --repo $TARGET_REPO..."
    if [[ "$DRY_RUN" -eq 1 ]]; then
      aaf gc --repo "$TARGET_REPO" --merged-branches --dry-run || true
    else
      aaf gc --repo "$TARGET_REPO" --merged-branches || true
    fi
  else
    log "Step 1: Running aaf gc --all..."
    if [[ "$DRY_RUN" -eq 1 ]]; then
      aaf gc --all --merged-branches --dry-run || true
    else
      aaf gc --all --merged-branches || true
    fi
  fi
else
  log "Step 1: 'aaf' binary not found in PATH, skipping built-in gc."
fi

removed_targets=0

# 2. Clean idle aaf-wt/target-* and aaf-* directories (skipped when --repo is specified)
if [[ -z "$TARGET_REPO" && -d "$AAF_WT_DIR" ]]; then
  log "Step 2: Checking idle directories in $AAF_WT_DIR (min age: ${MIN_AGE_MINS}m)..."
  for dir in "$AAF_WT_DIR"/target-* "$AAF_WT_DIR"/aaf-*; do
    [[ -d "$dir" && ! -L "$dir" ]] || continue
    base=$(basename "$dir")

    if dir_in_use "$dir"; then
      log "  SKIP (in use by process): $base"
      continue
    fi

    if dir_recently_modified "$dir"; then
      log "  SKIP (recently active < ${MIN_AGE_MINS}m): $base"
      continue
    fi

    # For aaf-* worktree clones, enforce full git clean, unlocked, and merged check (fail-closed)
    if [[ "$base" == aaf-* ]]; then
      if [[ ! -e "$dir/.git" ]]; then
        log "  SKIP (not a git worktree): $base"
        continue
      fi
      # Resolve parent main repo from git common dir
      common_dir=$(git -C "$dir" rev-parse --git-common-dir 2>/dev/null || true)
      parent_repo=""
      if [[ -n "$common_dir" ]]; then
        parent_repo=$(canonicalize_dir "$(dirname "$common_dir")")
      fi
      if [[ -n "$parent_repo" ]] && is_blacklisted "$parent_repo"; then
        log "  SKIP (belongs to blacklisted repo): $base"
        continue
      fi

      # Check lock status in parent repo if linked worktree
      if [[ -n "$parent_repo" && "$parent_repo" != "$(canonicalize_dir "$dir")" ]]; then
        if git -C "$parent_repo" worktree list --porcelain 2>/dev/null | awk -v target="$(canonicalize_dir "$dir")" '
          BEGIN { found=0; is_locked=0 }
          $0 == "worktree " target { found=1; next }
          /^worktree / { found=0 }
          found && /^locked/ { is_locked=1; exit 0 }
          END { exit (is_locked ? 0 : 1) }
        '; then
          log "  SKIP (locked): $base"
          continue
        fi
      fi

      dirty=$(git -C "$dir" status --porcelain 2>/dev/null || echo "ERR")
      if [[ "$dirty" == "ERR" || -n "$dirty" ]]; then
        log "  SKIP (dirty or unreadable git status): $base"
        continue
      fi

      base_ref=""
      for cand in origin/main main origin/master master origin/develop develop; do
        if git -C "$dir" rev-parse --verify "$cand" >/dev/null 2>&1; then
          base_ref="$cand"
          break
        fi
      done
      if [[ -z "$base_ref" ]] || ! git -C "$dir" merge-base --is-ancestor HEAD "$base_ref" 2>/dev/null; then
        log "  SKIP (unmerged commits or unknown base): $base"
        continue
      fi

      if [[ "$DRY_RUN" -eq 1 ]]; then
        log "  [DRY-RUN] Would remove idle worktree: $dir"
      else
        if [[ -n "$parent_repo" && "$parent_repo" != "$(canonicalize_dir "$dir")" ]]; then
          git -C "$parent_repo" worktree remove "$dir" 2>/dev/null || rm -rf "$dir"
          git -C "$parent_repo" worktree prune 2>/dev/null || true
        else
          rm -rf "$dir"
        fi
        log "  REMOVED $dir"
      fi
      removed_targets=$((removed_targets + 1))
      continue
    fi

    if [[ "$DRY_RUN" -eq 1 ]]; then
      log "  [DRY-RUN] Would remove idle target: $dir"
    else
      size=$(du -sh "$dir" 2>/dev/null | cut -f1 || echo "?")
      rm -rf "$dir"
      log "  REMOVED $dir ($size)"
    fi
    removed_targets=$((removed_targets + 1))
  done
fi

# 3. Discover repositories to clean (.claude/worktrees + merged local branches)
collect_repos() {
  if [[ -n "$TARGET_REPO" ]]; then
    echo "$TARGET_REPO"
    return
  fi

  while IFS= read -r gitdir; do
    dirname "$gitdir"
  done < <(find /Volumes/Data/Toan /Volumes/Data/GalaxyHolding /Volumes/Data/Projects -maxdepth 3 -name ".git" -type d 2>/dev/null | sort -u)
}

removed_wt=0
removed_branches=0

log "Step 3 & 4: Scanning repositories for clean merged worktrees & merged branches..."
while IFS= read -r repo; do
  [[ -n "$repo" && -d "$repo" ]] || continue
  repo=$(canonicalize_dir "$repo")

  if is_blacklisted "$repo"; then
    log "  SKIP blacklisted repo: $repo"
    continue
  fi

  base_branch=$(resolve_base_branch "$repo" || true)

  if [[ "$DRY_RUN" -eq 0 ]]; then
    git -C "$repo" worktree prune 2>/dev/null || true
  fi

  # 3a. Clean .claude/worktrees (only if base_branch resolved; fail-closed)
  if [[ -n "$base_branch" && -d "$repo/.claude/worktrees" ]]; then
    for wt in "$repo"/.claude/worktrees/*; do
      [[ -d "$wt" && ! -L "$wt" ]] || continue
      if worktree_safe_to_remove "$repo" "$wt" "$base_branch"; then
        if [[ "$DRY_RUN" -eq 1 ]]; then
          log "  [DRY-RUN] Would remove clean+merged worktree: $wt"
        else
          git -C "$repo" worktree remove "$wt" 2>/dev/null || {
            log "  SKIP (git worktree remove failed): $wt"
            continue
          }
          log "  REMOVED worktree: $wt"
        fi
        removed_wt=$((removed_wt + 1))
      fi
    done
    if [[ "$DRY_RUN" -eq 0 ]]; then
      git -C "$repo" worktree prune 2>/dev/null || true
    fi
  fi

  # 4. Delete local branches already merged into base_branch
  [[ -n "$base_branch" ]] || continue
  checked_out_branches=$(git -C "$repo" worktree list --porcelain 2>/dev/null | sed -n 's|^branch refs/heads/||p' || true)

  while IFS= read -r branch; do
    [[ -n "$branch" ]] || continue
    case "$branch" in
      main|master|develop|HEAD|release/*)
        continue
        ;;
    esac

    if printf '%s\n' "$checked_out_branches" | grep -Fxq -- "$branch"; then
      continue
    fi

    if [[ "$DRY_RUN" -eq 1 ]]; then
      log "  [DRY-RUN] Would delete merged branch in $(basename "$repo"): $branch (merged into $base_branch)"
    else
      if git -C "$repo" branch -d -- "$branch" >/dev/null 2>&1; then
        log "  DELETED merged branch in $(basename "$repo"): $branch"
      else
        log "  SKIP (git branch -d refused): $branch"
        continue
      fi
    fi
    removed_branches=$((removed_branches + 1))
  done < <(git -C "$repo" for-each-ref --format='%(refname:short)' --merged="$base_branch" refs/heads 2>/dev/null || true)

done < <(collect_repos)

# 5. Optional system/build caches (skipped when --repo is specified; only when tools are idle)
if [[ -z "$TARGET_REPO" && "$INCLUDE_CACHES" -eq 1 ]]; then
  log "Step 5: Cleaning safe system & build caches..."
  if [[ "$DRY_RUN" -eq 1 ]]; then
    log "  [DRY-RUN] Would clean Gradle build-cache-1/daemon, Xcode DerivedData, npm/uv caches (when respective tools are idle)"
  else
    if ! pgrep -f "gradle" >/dev/null 2>&1; then
      rm -rf ~/.gradle/daemon/* ~/.gradle/caches/build-cache-1 ~/.gradle/caches/journal-1 2>/dev/null || true
    else
      log "  SKIP Gradle cache cleanup (gradle process active)"
    fi
    if ! pgrep -f "xcodebuild|Xcode\.app" >/dev/null 2>&1; then
      rm -rf ~/Library/Developer/Xcode/DerivedData/* 2>/dev/null || true
    else
      log "  SKIP Xcode DerivedData cleanup (Xcode/xcodebuild active)"
    fi
    if ! pgrep -f "(^|/)(npm|uv)( |$)" >/dev/null 2>&1; then
      rm -rf ~/.npm/_cacache ~/.npm/_logs ~/.cache/uv 2>/dev/null || true
    else
      log "  SKIP npm/uv cache cleanup (npm/uv process active)"
    fi
  fi
fi

free_after=$(get_disk_free)
log "Summary: aaf_wt_matched=$removed_targets, worktrees_matched=$removed_wt, merged_branches_matched=$removed_branches"
log "Disk free after: $free_after (was: $free_before)"
