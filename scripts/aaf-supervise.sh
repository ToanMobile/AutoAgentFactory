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
#   status "refused"  `busy` waits silently for AAF_SUPERVISE_POLL s;
#                     `config` or `usage` notifies once via `aaf notify --message`
#                     (deduped per message until a clean exit) before waiting.
#                     `agent_run` is skipped (`aaf notify` refuses inside agent runs).
#   status "error"    runtime/config error (exit 1): notifies once, then backs off.
#   status "halted"   restarts with back-off; notifies once only when the run did
#                     not record a `factory_halted` event (`halt_recorded:true`).
#   crash (no status) notifies once via `aaf notify --message`, then backs off.
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

json_str() {
    printf '%s\n' "$out" \
        | grep -o "\"$1\":\"[^\"]*\"" \
        | tail -n 1 \
        | cut -d'"' -f4 \
        | cut -c1-300
}

last_msg=""
notify_once() {
    msg=$1
    if [ "$msg" != "$last_msg" ]; then
        last_msg=$msg
        log "notifying: $msg"
        "$AAF" notify --message "$msg" >/dev/null 2>&1 || true
    fi
}

delay=$MIN
runs=0
while :; do
    runs=$((runs + 1))
    out=$(keep_awake "$AAF" --json run --auto "$@")
    code=$?
    status=$(printf '%s\n' "$out" | grep -o '"status":"[a-z_]*"' | tail -n 1 | cut -d'"' -f4)
    refused_kind=$(printf '%s\n' "$out" | grep -o '"refused_kind":"[a-z_]*"' | tail -n 1 | cut -d'"' -f4)
    log "aaf run --auto ended: exit=$code status=${status:-none}"
    if [ "$code" -eq 0 ]; then
        last_msg=""
    else
        case "$status" in
            refused)
                case "$refused_kind" in
                    busy|agent_run)
                        # busy: another run holds the repo; agent_run: notify would also refuse.
                        ;;
                    config|usage)
                        detail=$(json_str reason)
                        [ -n "$detail" ] || detail="${refused_kind:-$status}"
                        notify_once "aaf-supervise: $REPO: run refused ($refused_kind): $detail"
                        ;;
                esac
                ;;
            error)
                detail=$(json_str error)
                [ -n "$detail" ] || detail="$status (exit $code)"
                notify_once "aaf-supervise: $REPO: aaf run error: $detail"
                ;;
            halted)
                if ! printf '%s\n' "$out" | grep -q '"halt_recorded":true'; then
                    detail=$(json_str halted)
                    [ -n "$detail" ] || detail="$status (exit $code)"
                    notify_once "aaf-supervise: $REPO: aaf run halted: $detail"
                fi
                ;;
            "")
                notify_once "aaf-supervise: $REPO: aaf run crashed (exit $code)"
                ;;
        esac
    fi
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
