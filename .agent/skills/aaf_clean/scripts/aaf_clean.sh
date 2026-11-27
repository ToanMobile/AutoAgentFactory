#!/usr/bin/env bash
set -euo pipefail
shopt -s nullglob

DRY_RUN=0
INCLUDE_CACHES=0
TARGET_REPO=""
MIN_AGE_MINS=120

BLACKLIST_REPO="/Volumes/Data/Toan/agent-workbench"
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
      TARGET_REPO="$2"
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
  --dry-run            Preview changes without deleting anything
  --repo <PATH>        Clean only a specific git repository
  --include-caches     Also clean Gradle build-cache/daemon, Xcode DerivedData, npm/uv caches (when idle)
  --min-age-mins <N>   Minimum idle age (minutes) before removing aaf-wt/target-* (default: 120)
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

# Fast, non-blocking check whether any active build/agent process holds files inside a directory
dir_in_use() {
  local dir="$1"
  local base
  base=$(basename "$dir")
  if pgrep -f "$base" >/dev/null 2>&1; then
    return 0
  fi
  if lsof -c cargo -c rustc -c aaf -c java -c gradle -Fn 2>/dev/null | grep -Fq "$dir"; then
    return 0
  fi
  return 1
}

# Check if any file inside dir was modified within MIN_AGE_MINS (fail-closed if recently touched)
dir_recently_modified() {
  local dir="$1"
  if [[ "$MIN_AGE_MINS" -eq 0 ]]; then
    return 1
  fi
  if find "$dir" -mmin -"$MIN_AGE_MINS" -print -quit 2>/dev/null | grep -q .; then
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
  return 1
}

# Fail-closed check: returns 0 ONLY if worktree is clean, unlocked, and all commits are merged into base_branch
worktree_safe_to_remove() {
  local repo="$1"
  local wt="$2"
  local base_branch="$3"

  # 1. Check if worktree is locked via porcelain output
  if git -C "$repo" worktree list --porcelain 2>/dev/null | awk -v target="$wt" '
    $0 == "worktree " target { found=1; next }
    /^worktree / { found=0 }
    found && /^locked/ { exit 0 }
    END { exit 1 }
  '; then
    log "  SKIP (locked): $wt"
    return 1
  fi

  # 2. Check if any running process is using this worktree
  if dir_in_use "$wt"; then
    log "  SKIP (in use by process): $wt"
    return 1
  fi

  # 3. Check dirty status (fail-closed if git status fails)
  local status_out
  if ! status_out=$(git -C "$wt" status --porcelain 2>/dev/null); then
    log "  SKIP (git status failed): $wt"
    return 1
  fi
  if [[ -n "$status_out" ]]; then
    return 1
  fi

  # 4. Check unmerged commits (fail-closed: must be ancestor OR 0 cherry '+' commits)
  local head_sha
  if ! head_sha=$(git -C "$wt" rev-parse --verify HEAD 2>/dev/null); then
    log "  SKIP (cannot resolve HEAD): $wt"
    return 1
  fi
  if git -C "$repo" merge-base --is-ancestor "$head_sha" "$base_branch" 2>/dev/null; then
    return 0
  fi
  local cherry_out
  if ! cherry_out=$(git -C "$repo" cherry "$base_branch" "$head_sha" 2>/dev/null); then
    return 1
  fi
  if printf '%s\n' "$cherry_out" | grep -q '^+'; then
    return 1
  fi
  return 0
}

free_before=$(df -h /System/Volumes/Data | awk 'NR==2 {print $4}')
log "Disk free before: $free_before (dry_run=$DRY_RUN)"

# 1. Run built-in aaf gc
if command -v aaf >/dev/null 2>&1; then
  log "Step 1: Running aaf gc..."
  if [[ "$DRY_RUN" -eq 1 ]]; then
    aaf gc --all --merged-branches --dry-run || true
  else
    aaf gc --all --merged-branches || true
  fi
else
  log "Step 1: 'aaf' binary not found in PATH, skipping built-in gc."
fi

# 2. Clean idle aaf-wt/target-* and aaf-* directories
if [[ -d "$AAF_WT_DIR" ]]; then
  log "Step 2: Checking idle directories in $AAF_WT_DIR (min age: ${MIN_AGE_MINS}m)..."
  for dir in "$AAF_WT_DIR"/target-* "$AAF_WT_DIR"/aaf-*; do
    [[ -d "$dir" ]] || continue
    base=$(basename "$dir")

    if dir_in_use "$dir"; then
      log "  SKIP (in use by process): $base"
      continue
    fi

    if dir_recently_modified "$dir"; then
      log "  SKIP (recently active < ${MIN_AGE_MINS}m): $base"
      continue
    fi

    # For aaf-* worktree clones, enforce full git clean + merged check (fail-closed)
    if [[ "$base" == aaf-* ]]; then
      if [[ ! -e "$dir/.git" ]]; then
        log "  SKIP (not a git worktree): $base"
        continue
      fi
      dirty=$(git -C "$dir" status --porcelain 2>/dev/null || echo "ERR")
      if [[ "$dirty" == "ERR" || -n "$dirty" ]]; then
        log "  SKIP (dirty or unreadable git status): $base"
        continue
      fi
      base_ref=""
      for cand in origin/main main origin/master master; do
        if git -C "$dir" rev-parse --verify "$cand" >/dev/null 2>&1; then
          base_ref="$cand"
          break
        fi
      done
      if [[ -z "$base_ref" ]] || ! git -C "$dir" merge-base --is-ancestor HEAD "$base_ref" 2>/dev/null; then
        log "  SKIP (unmerged commits or unknown base): $base"
        continue
      fi
    fi

    if [[ "$DRY_RUN" -eq 1 ]]; then
      log "  [DRY-RUN] Would remove idle directory: $dir"
    else
      size=$(du -sh "$dir" 2>/dev/null | cut -f1)
      rm -rf "$dir"
      log "  REMOVED $dir ($size)"
    fi
  done
fi

# 3. Discover repositories to clean (.claude/worktrees + merged local branches)
collect_repos() {
  if [[ -n "$TARGET_REPO" ]]; then
    echo "$TARGET_REPO"
    return
  fi

  find /Volumes/Data/Toan /Volumes/Data/GalaxyHolding /Volumes/Data/Projects -maxdepth 3 -name ".git" -type d 2>/dev/null \
    | xargs -I{} dirname "{}" \
    | sort -u
}

removed_wt=0
removed_branches=0

log "Step 3 & 4: Scanning repositories for clean merged worktrees & merged branches..."
while IFS= read -r repo; do
  [[ -n "$repo" && -d "$repo" ]] || continue

  # Strictly skip blacklisted repo
  if [[ "$repo" == "$BLACKLIST_REPO" || "$repo" == "$BLACKLIST_REPO/"* ]]; then
    log "  SKIP blacklisted repo: $repo"
    continue
  fi

  base_branch=$(resolve_base_branch "$repo" || true)
  [[ -n "$base_branch" ]] || continue

  git -C "$repo" worktree prune 2>/dev/null || true

  # 3a. Clean .claude/worktrees (fail-closed: clean, unlocked, idle, and fully merged)
  if [[ -d "$repo/.claude/worktrees" ]]; then
    for wt in "$repo"/.claude/worktrees/*; do
      [[ -d "$wt" ]] || continue
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
    git -C "$repo" worktree prune 2>/dev/null || true
  fi

  # 4. Delete local branches already merged into base_branch
  # Build list of branches currently checked out in any worktree
  checked_out_branches=$(git -C "$repo" worktree list --porcelain 2>/dev/null | sed -n 's|^branch refs/heads/||p' || true)

  while IFS= read -r branch; do
    [[ -n "$branch" ]] || continue
    case "$branch" in
      main|master|develop|HEAD|release/*)
        continue
        ;;
    esac

    # Skip if checked out in main or any linked worktree
    if printf '%s\n' "$checked_out_branches" | grep -Fxq "$branch"; then
      continue
    fi

    if [[ "$DRY_RUN" -eq 1 ]]; then
      log "  [DRY-RUN] Would delete merged branch in $(basename "$repo"): $branch (merged into $base_branch)"
    else
      if git -C "$repo" branch -d "$branch" >/dev/null 2>&1; then
        log "  DELETED merged branch in $(basename "$repo"): $branch"
      else
        log "  SKIP (git branch -d refused): $branch"
        continue
      fi
    fi
    removed_branches=$((removed_branches + 1))
  done < <(git -C "$repo" for-each-ref --format='%(refname:short)' --merged="$base_branch" refs/heads 2>/dev/null || true)

done < <(collect_repos)

# 5. Optional system/build caches (only when respective tools are idle)
if [[ "$INCLUDE_CACHES" -eq 1 ]]; then
  log "Step 5: Cleaning safe system & build caches..."
  if [[ "$DRY_RUN" -eq 1 ]]; then
    log "  [DRY-RUN] Would clean Gradle build-cache-1 & daemon (if gradle idle), Xcode DerivedData, npm/uv caches"
  else
    if ! pgrep -f "gradle" >/dev/null 2>&1; then
      rm -rf ~/.gradle/daemon/* ~/.gradle/caches/build-cache-1 ~/.gradle/caches/journal-1 2>/dev/null || true
    else
      log "  SKIP Gradle cache cleanup (gradle process active)"
    fi
    rm -rf ~/Library/Developer/Xcode/DerivedData/* 2>/dev/null || true
    rm -rf ~/.npm/_cacache ~/.npm/_logs ~/.cache/uv 2>/dev/null || true
  fi
fi

free_after=$(df -h /System/Volumes/Data | awk 'NR==2 {print $4}')
log "Summary: worktrees_matched=$removed_wt, merged_branches_matched=$removed_branches"
log "Disk free after: $free_after (was: $free_before)"
