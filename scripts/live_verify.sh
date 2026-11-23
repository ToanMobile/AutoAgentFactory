#!/usr/bin/env bash
# Non-destructive empirical live verification harness for AutoAgentFactory (aaf).
# Verifies macOS Seatbelt confinement, real Android Gradle / Unity EditMode
# targets, GitHub CLI + `aaf board --dry-run`, and `aaf-hook` PreToolUse guards.
#
# Safety guarantees:
# - Uses isolated temporary AAF_DATA_DIR and AAF_CONFIG_DIR under /tmp.
# - Never mutates tracked/untracked files in target repositories (optional
#   Gradle unit-test runs execute in a disposable git worktree removed on EXIT).
# - Never pushes to GitHub or creates/mutates GitHub issues, PRs, or Projects.
# - Cleans up all temporary files and worktrees automatically via `trap EXIT`.

set -euo pipefail

ANDROID_REPO=""
ANDROID_UNIT_TASK=""
UNITY_REPO=""
GITHUB_REPO=""
SKIP_ANDROID=0
SKIP_UNITY=0
SKIP_GITHUB=0
UNITY_TIMEOUT_SECS=150

AAF_BIN="${AAF_BIN:-$(command -v aaf || echo "$HOME/.cargo/bin/aaf")}"
AAF_HOOK_BIN="${AAF_HOOK_BIN:-$(command -v aaf-hook || echo "$HOME/.cargo/bin/aaf-hook")}"
GH_BIN="${GH_BIN:-$(command -v gh || echo "/opt/homebrew/bin/gh")}"
SANDBOX_EXEC="/usr/bin/sandbox-exec"

usage() {
  cat <<'EOF'
Usage: scripts/live_verify.sh [OPTIONS]

Options:
  --android <path>              Android Gradle repository path (read-only target)
  --android-unit-test <task>    Optional Gradle task (e.g. :core:common:testDebugUnitTest)
                                executed in a disposable git worktree inside Seatbelt
  --unity <path>                Unity repository path (read-only target; logs to /tmp)
  --github-repo <owner/repo>    GitHub repo for read-only gh & --dry-run board checks
                                (default: ToanMobile/AutoAgentFactory)
  --skip-android                Skip Android Gradle + aaf doctor checks
  --skip-unity                  Skip Unity EditMode checks
  --skip-github                 Skip GitHub CLI + aaf board --dry-run checks
  -h, --help                    Show this help message
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --android)
      ANDROID_REPO="${2:?missing path for --android}"
      shift 2
      ;;
    --android-unit-test)
      ANDROID_UNIT_TASK="${2:?missing task for --android-unit-test}"
      shift 2
      ;;
    --unity)
      UNITY_REPO="${2:?missing path for --unity}"
      shift 2
      ;;
    --github-repo)
      GITHUB_REPO="${2:?missing owner/repo for --github-repo}"
      shift 2
      ;;
    --skip-android)
      SKIP_ANDROID=1
      shift
      ;;
    --skip-unity)
      SKIP_UNITY=1
      shift
      ;;
    --skip-github)
      SKIP_GITHUB=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ -z "$GITHUB_REPO" ]]; then
  GITHUB_REPO="ToanMobile/AutoAgentFactory"
fi

# Temp workspace and disposable worktree tracking for trap cleanup.
TMP_ROOT="$(mktemp -d /tmp/aaf_live_verify_XXXXXX)"
TMP_ROOT_REAL="$(cd "$TMP_ROOT" && pwd -P)"
DISPOSABLE_WT_REPO=""
DISPOSABLE_WT_PATH=""

cleanup() {
  if [[ -n "$DISPOSABLE_WT_REPO" && -n "$DISPOSABLE_WT_PATH" ]]; then
    git -C "$DISPOSABLE_WT_REPO" worktree remove --force "$DISPOSABLE_WT_PATH" >/dev/null 2>&1 || true
    git -C "$DISPOSABLE_WT_REPO" worktree prune >/dev/null 2>&1 || true
  fi
  rm -rf "$TMP_ROOT" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# Summary table storage
CHECK_NAMES=()
CHECK_TARGETS=()
CHECK_RESULTS=()
CHECK_DURATIONS=()
CHECK_NOTES=()
FAIL_COUNT=0
PASS_COUNT=0
SKIP_COUNT=0

now_ms() {
  python3 -c 'import time; print(int(time.monotonic() * 1000))'
}

format_ms() {
  local ms="$1"
  if (( ms >= 1000 )); then
    python3 -c "print(f'{${ms}/1000:.2f}s')"
  else
    echo "${ms}ms"
  fi
}

record_result() {
  local name="$1"
  local target="$2"
  local result="$3"
  local duration="$4"
  local note="$5"
  CHECK_NAMES+=("$name")
  CHECK_TARGETS+=("$target")
  CHECK_RESULTS+=("$result")
  CHECK_DURATIONS+=("$duration")
  CHECK_NOTES+=("$note")
  case "$result" in
    PASS) PASS_COUNT=$((PASS_COUNT + 1)) ;;
    FAIL) FAIL_COUNT=$((FAIL_COUNT + 1)) ;;
    SKIP) SKIP_COUNT=$((SKIP_COUNT + 1)) ;;
  esac
  printf '[%-4s] %-34s (%s) %s\n' "$result" "$name" "$duration" "$note"
}

canonical_path() {
  python3 -c 'import pathlib, sys; print(pathlib.Path(sys.argv[1]).resolve())' "$1"
}

# Build a Seatbelt profile faithfully matching crates/aaf-engine/src/executors/{sandbox,mod}.rs
write_aaf_sbpl() {
  local out_file="$1"
  local worktree="$2"        # empty string means no writable worktree
  local data_dir="$3"
  local extra_writable="${4:-}"
  local home_real
  home_real="$(canonical_path "$HOME")"
  local tmpdir_real
  tmpdir_real="$(canonical_path "${TMPDIR:-/tmp}")"
  local sibling_c=""
  if [[ "$(basename "$tmpdir_real")" == "T" ]]; then
    sibling_c="$(dirname "$tmpdir_real")/C"
  fi

  cat >"$out_file" <<EOF
(version 1)
(allow default)
(deny file-write*)
(allow file-write*
  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/tty")
  (literal "/dev/ptmx")
  (literal "/dev/dtracehelper")
  (regex #"^/dev/ttys[0-9]+\$")
  (regex #"^/dev/fd/[0-9]+\$"))
(allow file-write* (subpath "/private/tmp"))
(allow file-write* (subpath "/private/var/tmp"))
(allow file-write* (subpath "$tmpdir_real"))
EOF
  if [[ -n "$sibling_c" ]]; then
    printf '(allow file-write* (subpath "%s"))\n' "$sibling_c" >>"$out_file"
  fi

  local cache_dirs=(
    ".cargo" ".rustup" ".gradle" ".m2" ".android" ".npm" ".yarn"
    ".pub-cache" ".swiftpm" ".cocoapods" "go/pkg/mod" ".cache/pip"
    ".cache/uv" ".cache/go-build" ".cache/sccache" "Library/Caches/pip"
    "Library/Caches/Yarn" "Library/Caches/pnpm" "Library/Caches/go-build"
    "Library/Caches/CocoaPods" "Library/Caches/org.swift.swiftpm"
    "Library/Developer/Xcode/DerivedData" ".konan"
    "Library/Application Support/kotlin" "Library/Unity"
    "Library/Caches/com.unity3d.UnityEditor" "Library/Caches/Unity"
    "Library/Logs/Unity" "Library/Application Support/Unity"
    "Library/Developer/CoreSimulator" "Library/Caches/com.apple.dt.Xcode"
    "Library/org.swift.swiftpm"
  )
  local rel
  for rel in "${cache_dirs[@]}"; do
    printf '(allow file-write* (subpath "%s/%s"))\n' "$home_real" "$rel" >>"$out_file"
  done

  if [[ -n "${ANDROID_HOME:-}" ]]; then
    printf '(allow file-write* (subpath "%s"))\n' "$(canonical_path "$ANDROID_HOME")" >>"$out_file"
  fi
  if [[ -n "${ANDROID_SDK_ROOT:-}" && "${ANDROID_SDK_ROOT:-}" != "${ANDROID_HOME:-}" ]]; then
    printf '(allow file-write* (subpath "%s"))\n' "$(canonical_path "$ANDROID_SDK_ROOT")" >>"$out_file"
  fi
  if [[ -n "$extra_writable" ]]; then
    printf '(allow file-write* (subpath "%s"))\n' "$extra_writable" >>"$out_file"
  fi

  if [[ -n "$worktree" ]]; then
    local wt_real
    wt_real="$(canonical_path "$worktree")"
    printf '(allow file-write* (subpath "%s"))\n' "$wt_real" >>"$out_file"
    printf '(deny file-write* (literal "%s"))\n' "$wt_real" >>"$out_file"
    printf '(deny file-write* (literal "%s/.git"))\n' "$wt_real" >>"$out_file"
  fi

  # CACHE_WRITE_DENIED from crates/aaf-engine/src/executors/mod.rs
  cat >>"$out_file" <<EOF
(deny file-write* (subpath "$home_real/.gradle/init.d"))
(deny file-write* (subpath "$home_real/.gradle/gradle.properties"))
(deny file-write* (regex #"^$home_real/\\.gradle/init\\.gradle"))
(deny file-write* (subpath "$home_real/.cargo/bin"))
(deny file-write* (subpath "$home_real/.cargo/env"))
(deny file-write* (regex #"^$home_real/\\.cargo/config"))
(deny file-write* (regex #"^$home_real/\\.cargo/credentials"))
(deny file-write* (regex #"^$home_real/\\.android/adbkey"))
(deny file-write* (subpath "$home_real/.android/debug.keystore"))
(deny file-write* (subpath "$home_real/.m2/settings.xml"))
(deny file-write* (subpath "$home_real/.m2/settings-security.xml"))
(deny file-write* (subpath "$home_real/.pub-cache/bin"))
(deny file-write* (subpath "$home_real/.pub-cache/credentials.json"))
(deny file-write* (literal "$home_real/.gradle"))
(deny file-write* (literal "$home_real/.android"))
(deny file-write* (literal "$home_real/.cargo"))
(deny file-write* (literal "$home_real/.m2"))
(deny file-write* (literal "$home_real/.pub-cache"))
EOF

  if [[ -n "${ANDROID_HOME:-}" ]]; then
    local sdk_real
    sdk_real="$(canonical_path "$ANDROID_HOME")"
    printf '(deny file-write* (subpath "%s/platform-tools"))\n' "$sdk_real" >>"$out_file"
    printf '(deny file-write* (literal "%s"))\n' "$sdk_real" >>"$out_file"
  fi

  # AAF_DATA_DIR read/write denied, with throttle/ and throttle/guest/ reopened
  local data_real
  data_real="$(canonical_path "$data_dir")"
  cat >>"$out_file" <<EOF
(deny file-read* file-write* (subpath "$data_real"))
(allow file-read* (subpath "$data_real/throttle"))
(allow file-read* file-write* (subpath "$data_real/throttle/guest"))
(deny file-write* (literal "$data_real/throttle/guest"))
EOF
}

# Prepare isolated AAF_DATA_DIR throttle queue (matching open_queue / prepare_for_guests)
prepare_throttle_dir() {
  local data_dir="$1"
  mkdir -p -m 700 "$data_dir"
  AAF_DATA_DIR="$data_dir" "$AAF_BIN" slot -- /usr/bin/true >/dev/null 2>&1
  touch "$data_dir/throttle/drain.lock"
  chmod 600 "$data_dir/throttle/drain.lock"
}

echo "=== AutoAgentFactory Live Verification Harness ==="
echo "OS:      $(sw_vers -productName 2>/dev/null || uname -s) $(sw_vers -productVersion 2>/dev/null || uname -r) ($(uname -m))"
echo "aaf:     $("$AAF_BIN" --version 2>/dev/null || echo "$AAF_BIN")"
echo "hook:    $AAF_HOOK_BIN"
echo "sandbox: $SANDBOX_EXEC"
echo ""

# ---------------------------------------------------------------------------
# 1. Seatbelt Isolation Core Check
# ---------------------------------------------------------------------------
check_seatbelt_core() {
  local t0 t1 dur
  t0="$(now_ms)"
  if [[ ! -x "$SANDBOX_EXEC" ]]; then
    record_result "seatbelt-core" "sandbox-exec" "SKIP" "0ms" "sandbox-exec not available (non-macOS)"
    return
  fi

  local work="$TMP_ROOT_REAL/exp1"
  local wt="$work/worktree"
  local outside="$work/outside"
  local data="$work/aaf_data"
  mkdir -p "$wt" "$outside" "$data"
  chmod 700 "$data"
  printf 'SECRET_RECEIPT_KEY\n' >"$data/receipt.key"
  chmod 600 "$data/receipt.key"

  local sbpl="$work/core.sb"
  cat >"$sbpl" <<EOF
(version 1)
(allow default)
(deny file-write*)
(allow file-write*
  (literal "/dev/null") (literal "/dev/zero") (literal "/dev/tty")
  (literal "/dev/ptmx") (literal "/dev/dtracehelper")
  (regex #"^/dev/ttys[0-9]+\$") (regex #"^/dev/fd/[0-9]+\$"))
(allow file-write* (subpath "$wt"))
(deny file-read* file-write* (subpath "$data"))
EOF

  local ok=1
  local notes=()

  if "$SANDBOX_EXEC" -f "$sbpl" -- /bin/cat "$data/receipt.key" >/dev/null 2>&1; then
    ok=0
    notes+=("receipt.key read was NOT denied")
  fi
  if "$SANDBOX_EXEC" -f "$sbpl" -- /bin/sh -c "echo x > '$outside/escape.txt'" >/dev/null 2>&1; then
    ok=0
    notes+=("outside write was NOT denied")
  fi
  if ! "$SANDBOX_EXEC" -f "$sbpl" -- /bin/sh -c "echo ok > '$wt/ok.txt'" >/dev/null 2>&1; then
    ok=0
    notes+=("worktree write failed")
  fi
  if ! "$SANDBOX_EXEC" -f "$sbpl" -- /bin/sh -c "echo ok > /dev/null" >/dev/null 2>&1; then
    ok=0
    notes+=("/dev/null write failed")
  fi

  t1="$(now_ms)"
  dur="$(format_ms $((t1 - t0)))"
  if (( ok == 1 )); then
    record_result "seatbelt-core" "tmp-worktree" "PASS" "$dur" "receipt.key EPERM, outside EPERM, worktree+/dev/null OK"
  else
    record_result "seatbelt-core" "tmp-worktree" "FAIL" "$dur" "${notes[*]}"
  fi
}

# ---------------------------------------------------------------------------
# 2. Android Gradle inside macOS Seatbelt + aaf doctor
# ---------------------------------------------------------------------------
check_android() {
  if (( SKIP_ANDROID == 1 )); then
    record_result "android-doctor" "skipped" "SKIP" "0ms" "--skip-android"
    record_result "android-gradle-seatbelt" "skipped" "SKIP" "0ms" "--skip-android"
    return
  fi
  if [[ -z "$ANDROID_REPO" ]]; then
    record_result "android-doctor" "none" "SKIP" "0ms" "pass --android <path> to enable"
    record_result "android-gradle-seatbelt" "none" "SKIP" "0ms" "pass --android <path> to enable"
    return
  fi
  if [[ ! -d "$ANDROID_REPO" || ! -x "$ANDROID_REPO/gradlew" ]]; then
    record_result "android-gradle-seatbelt" "$ANDROID_REPO" "FAIL" "0ms" "repo or ./gradlew not found"
    return
  fi

  local status_before status_after
  status_before="$(git -C "$ANDROID_REPO" status --short)"

  local work="$TMP_ROOT_REAL/exp2"
  local data="$work/data"
  local cfg="$work/cfg"
  mkdir -p -m 700 "$data" "$cfg"

  # 2a: aaf doctor --json
  local t0 t1 dur doc_out doc_rc=0
  t0="$(now_ms)"
  doc_out="$(cd "$ANDROID_REPO" && AAF_DATA_DIR="$data" AAF_CONFIG_DIR="$cfg" "$AAF_BIN" doctor --json 2>&1)" || doc_rc=$?
  t1="$(now_ms)"
  dur="$(format_ms $((t1 - t0)))"
  if (( doc_rc == 0 )) && echo "$doc_out" | grep -q '"ok":true'; then
    record_result "android-doctor" "$(basename "$ANDROID_REPO")" "PASS" "$dur" "all required doctor checks ok"
  else
    record_result "android-doctor" "$(basename "$ANDROID_REPO")" "FAIL" "$dur" "rc=$doc_rc"
  fi

  # 2b: aaf slot -- ./gradlew --no-daemon --version inside Seatbelt (repo READ-ONLY)
  prepare_throttle_dir "$data"
  printf 'DUMMY_KEY\n' >"$data/receipt.key"
  local sbpl="$work/android.sb"
  write_aaf_sbpl "$sbpl" "" "$data" ""

  t0="$(now_ms)"
  local gr_out gr_rc=0
  gr_out="$(cd "$ANDROID_REPO" && AAF_DATA_DIR="$data" "$SANDBOX_EXEC" -f "$sbpl" -- \
    "$AAF_BIN" slot -- ./gradlew --no-daemon --version 2>&1)" || gr_rc=$?
  t1="$(now_ms)"
  dur="$(format_ms $((t1 - t0)))"

  # Also verify CACHE_WRITE_DENIED (~/.gradle/init.d denied)
  local initd_denied=1
  if "$SANDBOX_EXEC" -f "$sbpl" -- /bin/sh -c "touch '$HOME/.gradle/init.d/.aaf_verify_probe'" >/dev/null 2>&1; then
    initd_denied=0
    rm -f "$HOME/.gradle/init.d/.aaf_verify_probe" || true
  fi

  if (( gr_rc == 0 && initd_denied == 1 )); then
    local gr_ver
    gr_ver="$(echo "$gr_out" | awk '/^Gradle /{print $2; exit}')"
    record_result "android-gradle-seatbelt" "$(basename "$ANDROID_REPO")" "PASS" "$dur" "Gradle ${gr_ver:-ok} via aaf slot; ~/.gradle/init.d EPERM"
  else
    record_result "android-gradle-seatbelt" "$(basename "$ANDROID_REPO")" "FAIL" "$dur" "rc=$gr_rc initd_denied=$initd_denied"
  fi

  # 2c: Optional Gradle unit test task in disposable git worktree inside Seatbelt
  if [[ -n "$ANDROID_UNIT_TASK" ]]; then
    local wt="$work/wt"
    DISPOSABLE_WT_REPO="$ANDROID_REPO"
    DISPOSABLE_WT_PATH="$wt"
    git -C "$ANDROID_REPO" worktree add --detach "$wt" HEAD >/dev/null 2>&1
    if [[ -f "$ANDROID_REPO/local.properties" ]]; then
      cp "$ANDROID_REPO/local.properties" "$wt/local.properties"
    fi
    local sbpl_wt="$work/android_wt.sb"
    write_aaf_sbpl "$sbpl_wt" "$wt" "$data" ""

    t0="$(now_ms)"
    local ut_rc=0
    (cd "$wt" && AAF_DATA_DIR="$data" "$SANDBOX_EXEC" -f "$sbpl_wt" -- \
      "$AAF_BIN" slot -- ./gradlew --no-daemon --max-workers=2 "$ANDROID_UNIT_TASK" \
      >"$work/gradle_ut.log" 2>&1) || ut_rc=$?
    t1="$(now_ms)"
    dur="$(format_ms $((t1 - t0)))"

    git -C "$ANDROID_REPO" worktree remove --force "$wt" >/dev/null 2>&1 || true
    git -C "$ANDROID_REPO" worktree prune >/dev/null 2>&1 || true
    DISPOSABLE_WT_REPO=""
    DISPOSABLE_WT_PATH=""

    if (( ut_rc == 0 )); then
      record_result "android-unit-test-seatbelt" "$ANDROID_UNIT_TASK" "PASS" "$dur" "disposable worktree BUILD SUCCESSFUL"
    else
      local tail_err
      tail_err="$(tail -n 3 "$work/gradle_ut.log" | tr '\n' ' ' | cut -c1-120)"
      record_result "android-unit-test-seatbelt" "$ANDROID_UNIT_TASK" "FAIL" "$dur" "rc=$ut_rc $tail_err"
    fi
  fi

  status_after="$(git -C "$ANDROID_REPO" status --short)"
  if [[ "$status_before" != "$status_after" ]]; then
    record_result "android-repo-clean" "$(basename "$ANDROID_REPO")" "FAIL" "0ms" "git status changed!"
  else
    record_result "android-repo-clean" "$(basename "$ANDROID_REPO")" "PASS" "0ms" "git status unchanged"
  fi
}

# ---------------------------------------------------------------------------
# 3. Unity EditMode inside macOS Seatbelt
# ---------------------------------------------------------------------------
check_unity() {
  if (( SKIP_UNITY == 1 )); then
    record_result "unity-version-seatbelt" "skipped" "SKIP" "0ms" "--skip-unity"
    record_result "unity-editmode-seatbelt" "skipped" "SKIP" "0ms" "--skip-unity"
    return
  fi
  if [[ -z "$UNITY_REPO" ]]; then
    record_result "unity-version-seatbelt" "none" "SKIP" "0ms" "pass --unity <path> to enable"
    record_result "unity-editmode-seatbelt" "none" "SKIP" "0ms" "pass --unity <path> to enable"
    return
  fi
  if [[ ! -f "$UNITY_REPO/ProjectSettings/ProjectVersion.txt" ]]; then
    record_result "unity-editmode-seatbelt" "$UNITY_REPO" "FAIL" "0ms" "ProjectSettings/ProjectVersion.txt missing"
    return
  fi

  local unity_ver unity_bin
  unity_ver="$(awk '/^m_EditorVersion:/{print $2; exit}' "$UNITY_REPO/ProjectSettings/ProjectVersion.txt")"
  unity_bin="${UNITY_PATH:-/Applications/Unity/Hub/Editor/${unity_ver}/Unity.app/Contents/MacOS/Unity}"
  if [[ ! -x "$unity_bin" ]]; then
    record_result "unity-editmode-seatbelt" "$UNITY_REPO" "SKIP" "0ms" "Unity $unity_ver binary not installed at $unity_bin"
    return
  fi

  local status_before status_after
  status_before="$(git -C "$UNITY_REPO" status --short)"

  local work="$TMP_ROOT_REAL/exp3"
  local data="$work/data"
  mkdir -p -m 700 "$data"
  prepare_throttle_dir "$data"
  printf 'DUMMY_KEY\n' >"$data/receipt.key"

  # 3a: Unity -version under strict aaf Seatbelt
  local sbpl_ver="$work/unity_ver.sb"
  write_aaf_sbpl "$sbpl_ver" "" "$data" ""
  local t0 t1 dur uv_out uv_rc=0
  t0="$(now_ms)"
  uv_out="$("$SANDBOX_EXEC" -f "$sbpl_ver" -- "$unity_bin" -version 2>&1)" || uv_rc=$?
  t1="$(now_ms)"
  dur="$(format_ms $((t1 - t0)))"
  if (( uv_rc == 0 )); then
    record_result "unity-version-seatbelt" "$(basename "$UNITY_REPO")" "PASS" "$dur" "Unity ${uv_out//$'\n'/ }"
  else
    record_result "unity-version-seatbelt" "$(basename "$UNITY_REPO")" "FAIL" "$dur" "rc=$uv_rc"
  fi

  # 3b: Unity EditMode tests under Seatbelt.
  # On macOS secondary volumes (/Volumes/<Vol>/...), Unity checks volume writability
  # via <mount>/.TemporaryItems/folders.<uid>. Detect and pass that path if applicable.
  local unity_real extra_tmp="" vol_note=""
  unity_real="$(canonical_path "$UNITY_REPO")"
  if [[ "$unity_real" =~ ^(/Volumes/[^/]+)/ ]]; then
    local vol_mount="${BASH_REMATCH[1]}"
    local uid_folder="${vol_mount}/.TemporaryItems/folders.$(id -u)"
    if [[ -d "$uid_folder" ]]; then
      extra_tmp="$uid_folder"
    else
      extra_tmp="${vol_mount}/.TemporaryItems"
    fi
    vol_note=" (+${extra_tmp})"
  fi

  local sbpl_em="$work/unity_em.sb"
  write_aaf_sbpl "$sbpl_em" "$unity_real" "$data" "$extra_tmp"

  local xml_out="$work/editmode.xml"
  local log_out="$work/unity.log"
  t0="$(now_ms)"
  local em_rc=0
  python3 - "$UNITY_TIMEOUT_SECS" "$SANDBOX_EXEC" "$sbpl_em" "$AAF_BIN" "$unity_bin" \
    "$unity_real" "$xml_out" "$log_out" "$data" <<'PY' || em_rc=$?
import os, subprocess, sys
timeout_s, sb, sbpl, aaf, unity, proj, xml_out, log_out, data_dir = sys.argv[1:]
env = os.environ.copy()
env["AAF_DATA_DIR"] = data_dir
try:
    p = subprocess.run(
        [sb, "-f", sbpl, "--", aaf, "slot", "--",
         unity, "-batchmode", "-projectPath", proj,
         "-runTests", "-testPlatform", "EditMode",
         "-testResults", xml_out, "-logFile", log_out],
        cwd=proj, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        timeout=int(timeout_s)
    )
    sys.exit(p.returncode)
except subprocess.TimeoutExpired:
    sys.exit(124)
PY
  t1="$(now_ms)"
  dur="$(format_ms $((t1 - t0)))"

  if (( em_rc == 0 )) && [[ -f "$xml_out" ]]; then
    local summary
    summary="$(python3 - "$xml_out" <<'PY'
import sys, xml.etree.ElementTree as ET
r = ET.parse(sys.argv[1]).getroot()
print("%s/%s passed, %s failed" % (
    r.attrib.get("passed", "?"),
    r.attrib.get("total", "?"),
    r.attrib.get("failed", "?"),
))
PY
)"
    record_result "unity-editmode-seatbelt" "$(basename "$UNITY_REPO")" "PASS" "$dur" "${summary}${vol_note}"
  else
    local reason="rc=$em_rc"
    if [[ -f "$log_out" ]] && grep -q "Project folder or disk is read only" "$log_out"; then
      reason="rc=$em_rc read-only volume check failed"
    elif (( em_rc == 124 )); then
      reason="timeout (${UNITY_TIMEOUT_SECS}s)"
    fi
    record_result "unity-editmode-seatbelt" "$(basename "$UNITY_REPO")" "FAIL" "$dur" "${reason}${vol_note}"
  fi

  status_after="$(git -C "$UNITY_REPO" status --short)"
  if [[ "$status_before" != "$status_after" ]]; then
    record_result "unity-repo-clean" "$(basename "$UNITY_REPO")" "FAIL" "0ms" "git status changed!"
  else
    record_result "unity-repo-clean" "$(basename "$UNITY_REPO")" "PASS" "0ms" "git status unchanged"
  fi
}

# ---------------------------------------------------------------------------
# 4. GitHub CLI & aaf board --dry-run (strictly read-only)
# ---------------------------------------------------------------------------
check_github() {
  if (( SKIP_GITHUB == 1 )); then
    record_result "github-auth-and-read" "skipped" "SKIP" "0ms" "--skip-github"
    record_result "github-board-dry-run" "skipped" "SKIP" "0ms" "--skip-github"
    return
  fi
  if [[ ! -x "$GH_BIN" ]]; then
    record_result "github-auth-and-read" "$GITHUB_REPO" "SKIP" "0ms" "gh binary not found"
    record_result "github-board-dry-run" "$GITHUB_REPO" "SKIP" "0ms" "gh binary not found"
    return
  fi

  local t0 t1 dur gh_rc=0 auth_out
  t0="$(now_ms)"
  auth_out="$("$GH_BIN" auth status 2>&1)" || gh_rc=$?
  if (( gh_rc == 0 )); then
    "$GH_BIN" repo view "$GITHUB_REPO" --json nameWithOwner >/dev/null 2>&1 || gh_rc=$?
  fi
  if (( gh_rc == 0 )); then
    "$GH_BIN" issue list --repo "$GITHUB_REPO" --limit 5 --json number >/dev/null 2>&1 || gh_rc=$?
  fi
  t1="$(now_ms)"
  dur="$(format_ms $((t1 - t0)))"
  if (( gh_rc == 0 )) && echo "$auth_out" | grep -q "'repo'" && echo "$auth_out" | grep -q "'project'"; then
    record_result "github-auth-and-read" "$GITHUB_REPO" "PASS" "$dur" "scopes repo+project, repo view & issue list OK"
  else
    record_result "github-auth-and-read" "$GITHUB_REPO" "FAIL" "$dur" "rc=$gh_rc (check gh auth status scopes)"
  fi

  # Disposable /tmp repo pointing at GITHUB_REPO for import-issues & sync-github --dry-run
  local work="$TMP_ROOT_REAL/exp4"
  local repo="$work/repo"
  local data="$work/data"
  local cfg="$work/cfg"
  mkdir -p "$repo/.autofactory" "$data" "$cfg"
  chmod 700 "$data" "$cfg"
  git -C "$repo" init -q
  git -C "$repo" remote add origin "git@github.com:${GITHUB_REPO}.git"

  local owner="${GITHUB_REPO%%/*}"
  cat >"$repo/.autofactory/config.toml" <<EOF
[project]
name = "${GITHUB_REPO##*/}"
profile = "rust"

[github]
enabled = false
repo = "${GITHUB_REPO}"
project_owner = "${owner}"
project_number = 0
label_filter = ""
EOF

  t0="$(now_ms)"
  local ok=1 notes=()

  # 4a: Fail-closed when owner_github_allowlist is empty
  local deny_rc=0
  (cd "$repo" && AAF_DATA_DIR="$data" AAF_CONFIG_DIR="$cfg" \
    "$AAF_BIN" --json board import-issues --dry-run >/dev/null 2>&1) || deny_rc=$?
  if (( deny_rc != 2 )); then
    ok=0
    notes+=("empty allowlist expected rc=2 got $deny_rc")
  fi

  # 4b: Allowlisted import-issues --dry-run
  cat >"$cfg/config.toml" <<EOF
[safety]
owner_github_allowlist = ["${owner}"]
EOF
  local imp_out imp_rc=0
  imp_out="$(cd "$repo" && AAF_DATA_DIR="$data" AAF_CONFIG_DIR="$cfg" \
    "$AAF_BIN" --json board import-issues --dry-run 2>&1)" || imp_rc=$?
  if (( imp_rc != 0 )) || ! echo "$imp_out" | grep -q '"dry_run":true'; then
    ok=0
    notes+=("import-issues --dry-run rc=$imp_rc")
  fi

  # 4c: Add local task to isolated temp DB + sync-github --dry-run
  local add_rc=0 sync_out sync_rc=0
  (cd "$repo" && AAF_DATA_DIR="$data" AAF_CONFIG_DIR="$cfg" \
    "$AAF_BIN" --json board add --title "Verify mapping" --spec "Dry-run" --accept "Mapped" >/dev/null 2>&1) || add_rc=$?
  sync_out="$(cd "$repo" && AAF_DATA_DIR="$data" AAF_CONFIG_DIR="$cfg" \
    "$AAF_BIN" --json board sync-github --dry-run 2>&1)" || sync_rc=$?
  if (( add_rc != 0 || sync_rc != 0 )) || ! echo "$sync_out" | grep -q '"dry_run":true'; then
    ok=0
    notes+=("sync-github --dry-run rc=$sync_rc")
  fi

  t1="$(now_ms)"
  dur="$(format_ms $((t1 - t0)))"
  if (( ok == 1 )); then
    record_result "github-board-dry-run" "$GITHUB_REPO" "PASS" "$dur" "empty-allowlist rc=2; import-issues & sync-github --dry-run rc=0"
  else
    record_result "github-board-dry-run" "$GITHUB_REPO" "FAIL" "$dur" "${notes[*]}"
  fi
}

# ---------------------------------------------------------------------------
# 5. aaf-hook PreToolUse end-to-end smoke checks
# ---------------------------------------------------------------------------
check_hook() {
  local work="$TMP_ROOT_REAL/exp5"
  local repo="$work/repo"
  local data="$work/data"
  local cfg="$work/cfg"
  mkdir -p "$repo" "$data" "$cfg"
  chmod 700 "$data" "$cfg"
  git -C "$repo" init -q

  local t0 t1 dur
  t0="$(now_ms)"
  local hook_rc=0 hook_note
  hook_note="$(python3 - "$AAF_HOOK_BIN" "$repo" "$data" "$cfg" <<'PY'
import json, os, subprocess, sys, time
hook_bin, repo, data, cfg = sys.argv[1:]
env = os.environ.copy()
env["AAF_DATA_DIR"] = data
env["AAF_CONFIG_DIR"] = cfg
cases = [
    ("git status", 0),
    ("aaf slot -- ./gradlew --version", 0),
    ("git push --force origin main", 2),
    ("rm .env", 2),
    ("gh pr merge 42 --squash", 2),
    ("./gradlew build", 2),
    ("aaf approve", 2),
]
latencies = []
failures = []
for cmd, expected in cases:
    payload = json.dumps({"tool_name": "Bash", "tool_input": {"command": cmd}, "cwd": repo}).encode()
    s = time.monotonic()
    p = subprocess.run([hook_bin], input=payload, cwd=repo, env=env, capture_output=True)
    latencies.append((time.monotonic() - s) * 1000)
    if p.returncode != expected:
        failures.append(f"{cmd}: rc={p.returncode}!=exp {expected}")
if failures:
    print("; ".join(failures))
    sys.exit(1)
print(f"{len(cases)}/{len(cases)} verdicts OK (min={min(latencies):.1f}ms max={max(latencies):.1f}ms)")
PY
  )" || hook_rc=$?
  t1="$(now_ms)"
  dur="$(format_ms $((t1 - t0)))"

  if (( hook_rc == 0 )); then
    record_result "aaf-hook-smoke" "7-payloads" "PASS" "$dur" "$hook_note"
  else
    record_result "aaf-hook-smoke" "7-payloads" "FAIL" "$dur" "$hook_note"
  fi
}

check_seatbelt_core
check_android
check_unity
check_github
check_hook

echo ""
echo "=== Summary ==="
printf '%-34s | %-20s | %-6s | %-9s | %s\n' "Check" "Target" "Result" "Duration" "Notes"
printf '%-34s-+-%-20s-+-%-6s-+-%-9s-+-%s\n' \
  "----------------------------------" "--------------------" "------" "---------" "----------------------------------------"
for i in "${!CHECK_NAMES[@]}"; do
  printf '%-34s | %-20s | %-6s | %-9s | %s\n' \
    "${CHECK_NAMES[$i]}" "${CHECK_TARGETS[$i]}" "${CHECK_RESULTS[$i]}" "${CHECK_DURATIONS[$i]}" "${CHECK_NOTES[$i]}"
done
echo ""
echo "Totals: PASS=$PASS_COUNT FAIL=$FAIL_COUNT SKIP=$SKIP_COUNT"

if (( FAIL_COUNT > 0 )); then
  exit 1
fi
exit 0
