#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `scripts/aaf-supervise.sh` with a stub `aaf`: a frozen factory is waited
//! out (polling `aaf status`) instead of restarted in a loop, and every other
//! exit is restarted.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

const STUB: &str = r#"#!/bin/sh
d="$SUPERVISE_STATE"
case "$*" in
  *"run --auto"*)
    n=$(cat "$d/runs" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "$d/runs"
    echo "$*" >> "$d/calls"
    s=$(sed -n "${n}p" "$d/plan")
    echo "{\"ok\":false,\"status\":\"$s\",\"command\":\"run\"}"
    [ "$s" = ok ] && exit 0
    exit 2 ;;
  *status*)
    c=$(cat "$d/polls" 2>/dev/null || echo 0); c=$((c + 1)); echo "$c" > "$d/polls"
    if [ "$c" -lt 3 ]; then echo '{"ok":true,"frozen":true}'; else echo '{"ok":true,"frozen":false}'; fi ;;
esac
"#;

fn read(dir: &Path, name: &str) -> String {
    fs::read_to_string(dir.join(name)).unwrap_or_default()
}

#[test]
fn frozen_is_waited_out_and_other_exits_are_restarted() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let stub = dir.join("aaf");
    fs::write(&stub, STUB).unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(dir.join("plan"), "frozen\nhalted\nok\n").unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/aaf-supervise.sh");

    let out = Command::new("/bin/sh")
        .arg(&script)
        .arg(dir)
        .arg("--unsandboxed")
        .env("AAF_BIN", &stub)
        .env("SUPERVISE_STATE", dir)
        .env("AAF_SUPERVISE_MIN", "0")
        .env("AAF_SUPERVISE_POLL", "0")
        .env("AAF_SUPERVISE_RUNS", "3")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");

    assert_eq!(read(dir, "runs").trim(), "3", "{stderr}");
    // Frozen: `aaf status` polled until it said unfrozen, then a new run.
    assert_eq!(read(dir, "polls").trim(), "3", "{stderr}");
    assert!(stderr.contains("status=frozen") && stderr.contains("waiting for"));
    assert!(stderr.contains("status=halted") && stderr.contains("restarting in"));
    // Extra flags reach `aaf run --auto`.
    assert!(
        read(dir, "calls")
            .lines()
            .all(|l| l.contains("--unsandboxed"))
    );
}

#[path = "factory_support.rs"]
mod support;

/// The JSON the script greps comes from the real binary: a frozen factory ends
/// the run with `"status":"frozen"` and `aaf status` says `"frozen":true`.
#[test]
fn the_real_binary_reports_frozen_the_way_the_script_reads_it() {
    use aaf_domain::ports::{KvStore, META_FACTORY_FROZEN};
    let fx = support::Fixture::new();
    fx.add("make a file");
    fx.db().set_meta(META_FACTORY_FROZEN, "1").unwrap();
    let status = fx.aaf(&["status"]);
    assert!(
        status.json.to_string().contains("\"frozen\":true"),
        "{}",
        status.json
    );

    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/aaf-supervise.sh");
    let mut command = fx.command(&fx.repo, &[]);
    command.env("AAF_BIN", env!("CARGO_BIN_EXE_aaf"));
    let program = command.get_program().to_owned();
    assert!(program.to_string_lossy().ends_with("aaf"));
    let out = Command::new("/bin/sh")
        .arg(&script)
        .arg(&fx.repo)
        .arg("--unsandboxed")
        .env_clear()
        .envs(command.get_envs().filter_map(|(k, v)| v.map(|v| (k, v))))
        .env("AAF_SUPERVISE_RUNS", "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("exit=2 status=frozen"), "{stderr}");
}
