#!/bin/sh
# Keeps `aaf run --auto` alive for one repository, unattended, and never
# busy-loops: it is what launchd/systemd start (see packaging/launchd and
# packaging/systemd).
#
# Usage:
#   sh scripts/aaf-supervise.sh <repo-path> [extra `aaf run` flags...]
#
# What it does when `aaf run --auto` ends:
#   status "frozen"   the factory froze after repeated failures. Waits, polling
#                     `aaf status` every AAF_SUPERVISE_POLL s, until a person runs
#                     `aaf board unfreeze`; then starts again. (`aaf notify` tells
#                     the owner; this script only refuses to spin.)
#   status "refused"  another run holds the repository: retries after
#                     AAF_SUPERVISE_POLL s.
#   anything else     a halt (login, network), a crash or a kill: restarts after
#                     a back-off that doubles from AAF_SUPERVISE_MIN up to
#                     AAF_SUPERVISE_MAX s, and resets after a clean exit.
# On macOS the run is wrapped in `caffeinate -is` so the machine does not sleep
# under it.
#
# Environment:
#   AAF_BIN               aaf binary (default: aaf on PATH)
#   AAF_SUPERVISE_MIN     first back-off in seconds (default 30)
#   AAF_SUPERVISE_MAX     back-off ceiling in seconds (default 1800)
#   AAF_SUPERVISE_POLL    frozen/refused poll interval in seconds (default 60)
#   AAF_SUPERVISE_RUNS    stop after this many runs (default: never; for tests)

set -u

if [ "$#" -lt 1 ]; then
    echo "usage: $0 <repo-path> [aaf run flags...]" >&2
    exit 2
fi
REPO=$1
shift

AAF="${AAF_BIN:-aaf}"
MIN="${AAF_SUPERVISE_MIN:-30}"
MAX="${AAF_SUPERVISE_MAX:-1800}"
POLL="${AAF_SUPERVISE_POLL:-60}"
RUNS="${AAF_SUPERVISE_RUNS:-0}"

cd "$REPO" || {
    echo "aaf-supervise: cannot enter $REPO" >&2
    exit 2
}

log() {
    echo "aaf-supervise: $(date '+%Y-%m-%dT%H:%M:%S') $*" >&2
}

keep_awake() {
    if command -v caffeinate >/dev/null 2>&1; then
        caffeinate -is "$@"
    else
        "$@"
    fi
}

is_frozen() {
    "$AAF" --json status 2>/dev/null | grep -q '"frozen":true'
}

delay=$MIN
runs=0
while :; do
    runs=$((runs + 1))
    out=$(keep_awake "$AAF" --json run --auto "$@")
    code=$?
    status=$(printf '%s\n' "$out" | grep -o '"status":"[a-z_]*"' | tail -n 1 | cut -d'"' -f4)
    log "aaf run --auto ended: exit=$code status=${status:-none}"
    if [ "$RUNS" -gt 0 ] && [ "$runs" -ge "$RUNS" ]; then
        exit 0
    fi
    case "$status" in
        frozen)
            log "factory frozen: waiting for \`aaf board unfreeze\`"
            while is_frozen; do
                sleep "$POLL"
            done
            delay=$MIN
            continue
            ;;
        refused)
            sleep "$POLL"
            continue
            ;;
    esac
    if [ "$code" -eq 0 ]; then
        delay=$MIN
    fi
    log "restarting in ${delay}s"
    sleep "$delay"
    delay=$((delay * 2))
    if [ "$delay" -gt "$MAX" ]; then
        delay=$MAX
    fi
done
