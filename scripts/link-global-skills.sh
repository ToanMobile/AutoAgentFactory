#!/usr/bin/env bash
# Link repo-owned AAF global skills into Antigravity (~/.gemini), Claude Code
# (~/.claude), and Codex (~/.codex) so nothing lives outside Git when you
# reinstall your machine.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"

if [[ ! -f "$REPO_ROOT/Cargo.toml" ]]; then
  echo "error: expected AutoAgentFactory repo root at $REPO_ROOT" >&2
  exit 1
fi

SKILLS=(
  "aaf-update:$REPO_ROOT/.agent/skills/aaf-update"
  "aaf-audit:$REPO_ROOT/.agent/skills/aaf-audit"
  "aaf_heath:$REPO_ROOT/.agent/skills/aaf_heath"
  "aaf-studio-reset:$REPO_ROOT/.agent/skills/aaf-studio-reset"
  "aaf-workflow:$REPO_ROOT/templates/skills/aaf-workflow"
)

HOST_DIRS=(
  "$HOME/.gemini/config/skills"
  "$HOME/.claude/skills"
  "$HOME/.codex/skills"
)

for host_dir in "${HOST_DIRS[@]}"; do
  mkdir -p "$host_dir"
  for entry in "${SKILLS[@]}"; do
    name="${entry%%:*}"
    src="${entry#*:}"
    if [[ ! -d "$src" ]]; then
      echo "warn: missing source skill dir $src (skipping $name)" >&2
      continue
    fi
    dst="$host_dir/$name"
    if [[ -L "$dst" ]]; then
      cur="$(readlink "$dst")"
      if [[ "$cur" == "$src" ]]; then
        continue
      fi
      rm -f "$dst"
    elif [[ -e "$dst" ]]; then
      # Replace old standalone copies that were previously left outside Git.
      rm -rf "$dst"
    fi
    ln -s "$src" "$dst"
    echo "linked: $dst -> $src"
  done
done

echo "Global AAF skills synced from $REPO_ROOT."
