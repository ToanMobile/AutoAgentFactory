#!/usr/bin/env bash
# Periodic heavy-stack benchmark runner for AutoAgentFactory (`aaf`).
#
# Modes:
#   --offline-rust (default) : deterministic offline 2-crate Rust workspace under
#                              /Volumes/Data/Toan/.aaf-audit-tmp/bench-heavy-$$
#                              measuring cold, warm-incremental, warm-receipt, hydrated.
#   --repo <path>            : Zero-Touch detached worktree benchmark for a real
#                              Gradle (gradlew), Xcode (*.xcworkspace/*.xcodeproj),
#                              or Cargo (Cargo.toml) repo, wrapped in host
#                              `aaf slot --label bench-heavy --`.
#
# Options:
#   --runs <N>               : iterations per scenario for median ms (default: 3)
#   --json                   : print machine-readable JSON output
#   --format <md|json|both>  : output format (default: md)
#   --cmd <command>          : override detected build/test command in --repo mode
#   --aaf-bin <path>         : explicit path to `aaf` binary
#   --ignore-pressure        : continue if macOS vm_pressure_level != 1
#   -h, --help               : show usage

set -euo pipefail

MODE="offline-rust"
TARGET_REPO=""
RUNS=3
OUTPUT_FORMAT="md"
CUSTOM_CMD=""
AAF_BIN="${AAF_BIN:-}"
IGNORE_PRESSURE=0

usage() {
  sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --offline-rust) MODE="offline-rust"; shift ;;
    --repo)
      [ "$#" -ge 2 ] || { echo "error: --repo requires a path" >&2; exit 2; }
      MODE="repo"
      TARGET_REPO="$2"
      shift 2
      ;;
    --runs)
      [ "$#" -ge 2 ] || { echo "error: --runs requires an integer" >&2; exit 2; }
      RUNS="$2"
      shift 2
      ;;
    --json) OUTPUT_FORMAT="json"; shift ;;
    --format)
      [ "$#" -ge 2 ] || { echo "error: --format requires md|json|both" >&2; exit 2; }
      OUTPUT_FORMAT="$2"
      shift 2
      ;;
    --cmd)
      [ "$#" -ge 2 ] || { echo "error: --cmd requires a command string" >&2; exit 2; }
      CUSTOM_CMD="$2"
      shift 2
      ;;
    --aaf-bin)
      [ "$#" -ge 2 ] || { echo "error: --aaf-bin requires a path" >&2; exit 2; }
      AAF_BIN="$2"
      shift 2
      ;;
    --ignore-pressure) IGNORE_PRESSURE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown option $1" >&2; usage >&2; exit 2 ;;
  esac
done

case "$RUNS" in
  ''|*[!0-9]*|0) echo "error: --runs must be a positive integer" >&2; exit 2 ;;
esac
case "$OUTPUT_FORMAT" in
  md|json|both) ;;
  *) echo "error: --format must be md, json, or both" >&2; exit 2 ;;
esac

TMP_PARENT="/Volumes/Data/Toan/.aaf-audit-tmp"
if [ ! -d "$(dirname "$TMP_PARENT")" ]; then
  TMP_PARENT="${TMPDIR:-/tmp}/.aaf-audit-tmp"
fi
mkdir -p "$TMP_PARENT"
BENCH_ROOT="${TMP_PARENT}/bench-heavy-$$"
DATA_DIR="${BENCH_ROOT}/data"
mkdir -p "$BENCH_ROOT" "$DATA_DIR"

CLEANUP_REPO=""
DETACHED_WTS=()

cleanup() {
  local wt
  if [ -n "$CLEANUP_REPO" ] && [ "${#DETACHED_WTS[@]}" -gt 0 ]; then
    for wt in "${DETACHED_WTS[@]}"; do
      git -C "$CLEANUP_REPO" worktree remove --force "$wt" >/dev/null 2>&1 || true
    done
    git -C "$CLEANUP_REPO" worktree prune >/dev/null 2>&1 || true
  fi
  rm -rf "$BENCH_ROOT" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

now_ms() {
  if command -v python3 >/dev/null 2>&1; then
    python3 -c 'import time; print(int(time.monotonic() * 1000))'
  elif command -v perl >/dev/null 2>&1; then
    perl -MTime::HiRes=time -e 'printf "%d\n", time() * 1000'
  else
    echo $(( $(date +%s) * 1000 ))
  fi
}

median_ms() {
  local sorted=()
  local val
  while IFS= read -r val; do
    [ -n "$val" ] && sorted+=("$val")
  done < <(printf '%s\n' "$@" | sort -n)
  local count="${#sorted[@]}"
  if [ "$count" -eq 0 ]; then
    echo 0
  else
    echo "${sorted[$((count / 2))]}"
  fi
}

cow_copy_dir() {
  local src="$1"
  local dst="$2"
  rm -rf "$dst"
  if [ "$(uname -s)" = "Darwin" ]; then
    /bin/cp -cRP -- "$src" "$dst" 2>/dev/null || /bin/cp -RP -- "$src" "$dst"
  else
    cp -a --reflink=auto --no-dereference -- "$src" "$dst"
  fi
}

discover_aaf_bin() {
  if [ -n "$AAF_BIN" ] && [ -x "$AAF_BIN" ]; then
    echo "$AAF_BIN"
    return 0
  fi
  local script_dir
  script_dir="$(cd "$(dirname "$0")" && pwd)"
  local cand
  for cand in \
    "${script_dir}/../target/release/aaf" \
    "${script_dir}/../target/debug/aaf" \
    "$(command -v aaf 2>/dev/null || true)"; do
    if [ -n "$cand" ] && [ -x "$cand" ]; then
      echo "$cand"
      return 0
    fi
  done
  echo ""
}

VM_PRESSURE="n/a"
check_vm_pressure() {
  if [ "$(uname -s)" = "Darwin" ]; then
    VM_PRESSURE="$(sysctl -n kern.memorystatus_vm_pressure_level 2>/dev/null || echo unknown)"
    if [ "$VM_PRESSURE" != "1" ] && [ "$IGNORE_PRESSURE" -ne 1 ]; then
      echo "error: macOS kern.memorystatus_vm_pressure_level=${VM_PRESSURE} (expected 1=normal); aborting benchmark (pass --ignore-pressure to override)" >&2
      exit 75
    fi
  fi
}

AAF_RESOLVED="$(discover_aaf_bin)"
SAFE_PATH="/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
prepend_safe_path_dir() {
  local dir="$1"
  if [ -n "$dir" ] && [ -d "$dir" ]; then
    case ":${SAFE_PATH}:" in
      *":${dir}:"*) ;;
      *) SAFE_PATH="${dir}:${SAFE_PATH}" ;;
    esac
  fi
}
prepend_safe_path_dir "${HOME}/.cargo/bin"
for tool in cargo rustc java xcodebuild; do
  tool_path="$(command -v "$tool" 2>/dev/null || true)"
  if [ -n "$tool_path" ]; then
    prepend_safe_path_dir "$(dirname "$tool_path")"
  fi
done
if [ -n "$AAF_RESOLVED" ]; then
  prepend_safe_path_dir "$(dirname "$AAF_RESOLVED")"
fi

REAL_HOME="$HOME"
mkdir -p "${BENCH_ROOT}"/{tmp,home,config,worktrees,warm-slots}
printf '[machine]\nmax_load_percent = 0\n' > "${BENCH_ROOT}/config/config.toml"

run_isolated() {
  local cwd="$1"
  shift
  (
    cd "$cwd"
    env -i \
      PATH="$SAFE_PATH" \
      HOME="${BENCH_ROOT}/home" \
      USER="${USER:-$(id -un)}" \
      LOGNAME="${LOGNAME:-$(id -un)}" \
      LANG="C.UTF-8" \
      TMPDIR="${BENCH_ROOT}/tmp" \
      AAF_DATA_DIR="$DATA_DIR" \
      AAF_CONFIG_DIR="${BENCH_ROOT}/config" \
      AAF_WORKTREE_DIR="${BENCH_ROOT}/worktrees" \
      AAF_WARM_SLOTS_DIR="${BENCH_ROOT}/warm-slots" \
      GIT_CONFIG_NOSYSTEM=1 \
      GIT_CONFIG_GLOBAL=/dev/null \
      CARGO_HOME="${CARGO_HOME:-$REAL_HOME/.cargo}" \
      RUSTUP_HOME="${RUSTUP_HOME:-$REAL_HOME/.rustup}" \
      GRADLE_USER_HOME="${GRADLE_USER_HOME:-$REAL_HOME/.gradle}" \
      JAVA_HOME="${JAVA_HOME:-}" \
      DEVELOPER_DIR="${DEVELOPER_DIR:-}" \
      ANDROID_HOME="${ANDROID_HOME:-}" \
      ANDROID_SDK_ROOT="${ANDROID_SDK_ROOT:-}" \
      "$@"
  )
}

time_isolated_ms() {
  local cwd="$1"
  shift
  local t0 t1 err_file
  err_file="${BENCH_ROOT}/tmp/cmd.err"
  t0="$(now_ms)"
  if ! run_isolated "$cwd" "$@" >"$err_file" 2>&1; then
    echo "bench command failed in $cwd: $*" >&2
    cat "$err_file" >&2
    return 1
  fi
  t1="$(now_ms)"
  echo $((t1 - t0))
}

# Results storage: parallel arrays for bash 3.2 compatibility on macOS.
RESULT_NAMES=()
RESULT_MEDIANS=()
RESULT_ALL=()
STACK_LABEL="rust-offline"

record_metric() {
  local name="$1"
  shift
  local med
  med="$(median_ms "$@")"
  RESULT_NAMES+=("$name")
  RESULT_MEDIANS+=("$med")
  local joined
  joined="$(IFS=,; echo "$*")"
  RESULT_ALL+=("$joined")
}

create_offline_rust_workspace() {
  local repo="$1"
  mkdir -p "$repo/.autofactory" "$repo/crates/core-lib/src" "$repo/crates/app-bin/src"

  cat > "$repo/.gitignore" <<'EOF'
target/
Cargo.lock
EOF

  cat > "$repo/Cargo.toml" <<'EOF'
[workspace]
members = ["crates/core-lib", "crates/app-bin"]
resolver = "2"
EOF

  cat > "$repo/crates/core-lib/Cargo.toml" <<'EOF'
[package]
name = "core-lib"
version = "0.1.0"
edition = "2021"
EOF

  cat > "$repo/crates/core-lib/src/lib.rs" <<'EOF'
pub fn compute(n: u64) -> u64 {
    (0..=n).fold(0u64, |acc, x| acc.wrapping_add(x.wrapping_mul(3)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deterministic_sum() {
        assert_eq!(compute(10), 165);
    }
}
EOF

  cat > "$repo/crates/app-bin/Cargo.toml" <<'EOF'
[package]
name = "app-bin"
version = "0.1.0"
edition = "2021"

[dependencies]
core-lib = { path = "../core-lib" }
EOF

  cat > "$repo/crates/app-bin/src/main.rs" <<'EOF'
fn main() {
    println!("{}", core_lib::compute(10));
}

#[cfg(test)]
mod tests {
    #[test]
    fn app_uses_core() {
        assert_eq!(core_lib::compute(4), 30);
    }
}
EOF

  cat > "$repo/.autofactory/config.toml" <<'EOF'
[project]
name = "bench-offline-rust"
profile = "rust"

[commands]
lint = "true"
test_scoped = "cargo test --workspace --offline"
test_full = "cargo test --workspace --offline"

[gate]
base_ref = "main"

[cross_audit]
require_different_provider = false

[worktree]
clone_dirs = ["target"]
EOF

  run_isolated "$repo" git init -q -b main
  run_isolated "$repo" git config user.name "AAF Bench"
  run_isolated "$repo" git config user.email "bench@local.invalid"
  run_isolated "$repo" git add -A
  run_isolated "$repo" git commit -q -m "initial offline rust workspace"
}

run_offline_rust_bench() {
  STACK_LABEL="rust-offline"
  local repo="${BENCH_ROOT}/offline-rust"
  create_offline_rust_workspace "$repo"
  CLEANUP_REPO="$repo"

  local cold_vals=() warm_inc_vals=() warm_rcpt_vals=() hyd_vals=()
  local i ms wt base_sha
  base_sha="$(run_isolated "$repo" git rev-parse HEAD)"

  for i in $(seq 1 "$RUNS"); do
    # 1. Cold build/test (clear both target/ and receipt store so each iteration is truly cold)
    rm -rf "$repo/target" "$DATA_DIR"
    mkdir -p "$DATA_DIR"
    if [ -n "$AAF_RESOLVED" ]; then
      ms="$(time_isolated_ms "$repo" "$AAF_RESOLVED" gate --scoped)"
    else
      ms="$(time_isolated_ms "$repo" cargo test --workspace --offline)"
    fi
    cold_vals+=("$ms")

    # 2. Warm incremental (1-line change in app-bin)
    printf '\n// iter %s\n' "$i" >> "$repo/crates/app-bin/src/main.rs"
    ms="$(time_isolated_ms "$repo" cargo test --workspace --offline)"
    warm_inc_vals+=("$ms")
    run_isolated "$repo" git checkout -- crates/app-bin/src/main.rs

    # 3. Warm receipt (repeat gate on unchanged tree)
    if [ -n "$AAF_RESOLVED" ]; then
      run_isolated "$repo" "$AAF_RESOLVED" gate --scoped >/dev/null 2>&1
      ms="$(time_isolated_ms "$repo" "$AAF_RESOLVED" gate --scoped)"
    else
      ms="$(time_isolated_ms "$repo" cargo test --workspace --offline)"
    fi
    warm_rcpt_vals+=("$ms")

    # 4. Hydrated detached worktree (`[worktree] clone_dirs = ["target"]` CoW)
    wt="${BENCH_ROOT}/wt-hyd-${i}"
    run_isolated "$repo" git worktree add -q --detach "$wt" "$base_sha"
    DETACHED_WTS+=("$wt")
    cow_copy_dir "$repo/target" "$wt/target"
    printf '\n// hyd %s\n' "$i" >> "$wt/crates/app-bin/src/main.rs"
    ms="$(time_isolated_ms "$wt" cargo test --workspace --offline)"
    hyd_vals+=("$ms")
    run_isolated "$repo" git worktree remove --force "$wt" >/dev/null 2>&1 || true
  done

  record_metric "cold" "${cold_vals[@]}"
  record_metric "warm-incremental" "${warm_inc_vals[@]}"
  record_metric "warm-receipt" "${warm_rcpt_vals[@]}"
  record_metric "hydrated" "${hyd_vals[@]}"
}

detect_real_stack() {
  local dir="$1"
  if [ -f "$dir/gradlew" ]; then
    echo "gradle"
  elif ls -d "$dir"/*.xcworkspace "$dir"/*.xcodeproj >/dev/null 2>&1; then
    echo "xcode"
  elif [ -f "$dir/Cargo.toml" ]; then
    echo "cargo"
  else
    echo "unknown"
  fi
}

run_real_repo_bench() {
  local abs_repo
  abs_repo="$(cd "$TARGET_REPO" && pwd -P)"
  case "$abs_repo" in
    /Volumes/Data/Toan/agent-workbench|/Volumes/Data/Toan/agent-workbench/*)
      echo "error: refusing blacklisted repository $abs_repo" >&2
      exit 2
      ;;
  esac
  git -C "$abs_repo" rev-parse --show-toplevel >/dev/null 2>&1 || {
    echo "error: $abs_repo is not a git repository" >&2
    exit 2
  }
  CLEANUP_REPO="$abs_repo"

  local base_sha
  base_sha="$(git -C "$abs_repo" rev-parse HEAD)"
  local wt="${BENCH_ROOT}/wt-main"
  git -C "$abs_repo" worktree add -q --detach "$wt" "$base_sha"
  DETACHED_WTS+=("$wt")

  local stack
  stack="$(detect_real_stack "$wt")"
  STACK_LABEL="$stack"

  local build_cmd="" artifact_dir=""
  case "$stack" in
    gradle)
      build_cmd="${CUSTOM_CMD:-./gradlew help}"
      artifact_dir="build"
      ;;
    xcode)
      build_cmd="${CUSTOM_CMD:-xcodebuild -list}"
      artifact_dir=".derived-data"
      ;;
    cargo)
      build_cmd="${CUSTOM_CMD:-cargo test --workspace}"
      artifact_dir="target"
      ;;
    *)
      echo "error: could not detect Gradle, Xcode, or Cargo in $abs_repo (use --cmd to specify)" >&2
      exit 2
      ;;
  esac

  local run_slot_cmd=()
  if [ -n "$AAF_RESOLVED" ]; then
    run_slot_cmd=("$AAF_RESOLVED" slot --label bench-heavy -- /bin/sh -c "$build_cmd")
  else
    run_slot_cmd=(/bin/sh -c "$build_cmd")
  fi

  local cold_vals=() warm_vals=() hyd_vals=()
  local i ms wt_hyd
  for i in $(seq 1 "$RUNS"); do
    rm -rf "${wt}/${artifact_dir}"
    ms="$(time_isolated_ms "$wt" "${run_slot_cmd[@]}")"
    cold_vals+=("$ms")

    ms="$(time_isolated_ms "$wt" "${run_slot_cmd[@]}")"
    warm_vals+=("$ms")

    wt_hyd="${BENCH_ROOT}/wt-hyd-${i}"
    git -C "$abs_repo" worktree add -q --detach "$wt_hyd" "$base_sha"
    DETACHED_WTS+=("$wt_hyd")
    if [ -d "${wt}/${artifact_dir}" ]; then
      cow_copy_dir "${wt}/${artifact_dir}" "${wt_hyd}/${artifact_dir}"
    fi
    ms="$(time_isolated_ms "$wt_hyd" "${run_slot_cmd[@]}")"
    hyd_vals+=("$ms")
    git -C "$abs_repo" worktree remove --force "$wt_hyd" >/dev/null 2>&1 || true
  done

  record_metric "cold" "${cold_vals[@]}"
  record_metric "warm" "${warm_vals[@]}"
  record_metric "hydrated" "${hyd_vals[@]}"
}

print_markdown() {
  echo "## Heavy-Stack Benchmark (${STACK_LABEL})"
  echo ""
  echo "- Mode: \`${MODE}\`"
  echo "- Runs per scenario: \`${RUNS}\`"
  echo "- macOS VM pressure level: \`${VM_PRESSURE}\`"
  echo "- \`aaf\` binary: \`${AAF_RESOLVED:-none (direct tool fallback)}\`"
  echo ""
  echo "| Scenario | Median (ms) | Samples (ms) |"
  echo "|---|---:|---|"
  local idx
  for idx in $(seq 0 $((${#RESULT_NAMES[@]} - 1))); do
    printf '| `%s` | %s | `%s` |\n' \
      "${RESULT_NAMES[$idx]}" "${RESULT_MEDIANS[$idx]}" "${RESULT_ALL[$idx]}"
  done
}

print_json() {
  local idx comma
  printf '{\n'
  printf '  "mode": "%s",\n' "$MODE"
  printf '  "stack": "%s",\n' "$STACK_LABEL"
  printf '  "runs": %s,\n' "$RUNS"
  printf '  "vm_pressure_level": "%s",\n' "$VM_PRESSURE"
  printf '  "aaf_bin": "%s",\n' "${AAF_RESOLVED:-}"
  printf '  "scenarios": [\n'
  for idx in $(seq 0 $((${#RESULT_NAMES[@]} - 1))); do
    comma=","
    [ "$idx" -eq $((${#RESULT_NAMES[@]} - 1)) ] && comma=""
    printf '    {"name": "%s", "median_ms": %s, "samples_ms": [%s]}%s\n' \
      "${RESULT_NAMES[$idx]}" "${RESULT_MEDIANS[$idx]}" "${RESULT_ALL[$idx]}" "$comma"
  done
  printf '  ]\n'
  printf '}\n'
}

check_vm_pressure
if [ "$MODE" = "repo" ]; then
  run_real_repo_bench
else
  run_offline_rust_bench
fi

case "$OUTPUT_FORMAT" in
  md) print_markdown ;;
  json) print_json ;;
  both)
    print_markdown
    echo ""
    print_json
    ;;
esac
