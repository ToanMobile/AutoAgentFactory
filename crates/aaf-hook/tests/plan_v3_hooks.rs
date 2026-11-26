#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Plan v3 Wave 1 Slice 1 regression suite:
//! - `human_guard` Tier-A / Tier-B split + flag escalations + `fallback.rs`
//! - `HOOK-04` (`cargo_run_args` scoped to `aaf-cli` workspaces)
//! - `HOOK-05` (`heavy_guard` gated on `in_factory_run` + static `trusted_aaf_bins`)
//! - `HOOK-06` (`.env.example`/`.env.sample`/`.env.template` allowed, symlink to `.env` denied)
//! - `HOOK-09` (`adb shell input swipe` / `*SwipeTest*` allowed, `wipe` commands denied)
//! - `HOOK-10` (local Gradle publish tasks exempted, remote publish still denied)
//! - `HOOK-11` (`rg`/`grep`/`git grep`/`git log`/`cargo test` static search patterns vs bypasses)
//! - `HOOK-14` (agent-actionable worker `FIX` strings)

mod common;

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use aaf_hook::Decision;
use common::{Fixture, run_corpus, run_file_corpus};
use serde_json::json;

const ALLOW: Option<&str> = None;
const HUMAN: Option<&str> = Some("human_action_guard");
const HEAVY: Option<&str> = Some("heavy_cmd_guard");
const DELETE: Option<&str> = Some("delete_guard");
const SECRET: Option<&str> = Some("secret_guard");
const ADB: Option<&str> = Some("adb_guard");
const STORE: Option<&str> = Some("store_submit_guard");
const DATA: Option<&str> = Some("data_dir_guard");
const OPAQUE: Option<&str> = Some("unparsable_command");

#[test]
fn interactive_allows_tier_b_and_blocks_tier_a_and_flag_escalations() {
    let fx = Fixture::new().interactive();
    run_corpus(
        &fx,
        &[
            // Tier-B commands allowed interactively (without escalated flags).
            ("aaf run", ALLOW),
            ("aaf run --once", ALLOW),
            ("aaf run --detach", ALLOW),
            ("aaf run 'fix login crash'", ALLOW),
            ("aaf run --land pr", ALLOW),
            ("aaf run --land=branch", ALLOW),
            ("aaf init", ALLOW),
            ("aaf init --with-skills", ALLOW),
            ("aaf plan", ALLOW),
            ("aaf gc", ALLOW),
            ("aaf gc --dry-run", ALLOW),
            ("aaf skills sync", ALLOW),
            ("aaf sop run deploy", ALLOW),
            ("aaf sop init", ALLOW),
            ("aaf board add --title 'fix' --accept 'tests pass'", ALLOW),
            ("aaf board ready T-1", ALLOW),
            ("aaf board split T-1", ALLOW),
            ("aaf board dep T-2 --after T-1", ALLOW),
            ("aaf board import-issues", ALLOW),
            ("aaf board sync-github", ALLOW),
            ("aaf board rework T-1 --note 'retry'", ALLOW),
            (
                "aaf merge 42 --expect-head 0123456789abcdef0123456789abcdef01234567 --dry-run",
                ALLOW,
            ),
            // Heavy builds allowed directly outside factory runs.
            ("./gradlew :app:testDebugUnitTest", ALLOW),
            ("xcodebuild -scheme App build", ALLOW),
            ("Unity -batchmode -runTests", ALLOW),
            // Fallback Tier-B & heavy tokens are gated on in_factory_run.
            ("aaf run 'oops", ALLOW),
            ("aaf init 'oops", ALLOW),
            ("./gradlew build 'oops", ALLOW),
            // Tier-A commands: ALWAYS blocked interactively.
            ("aaf approve", HUMAN),
            ("aaf approve --reviewer alice", HUMAN),
            (
                "aaf proof record --image screen.png --device emu-5554",
                HUMAN,
            ),
            ("aaf board unblock T-1", HUMAN),
            ("aaf board unfreeze", HUMAN),
            ("aaf board approve-plan T-1", HUMAN),
            ("aaf board undo T-1", HUMAN),
            ("aaf board steer T-1 'keep small'", HUMAN),
            ("aaf sop approve run-1 --reviewer alice", HUMAN),
            ("aaf sop send-back run-1 --to build --reason fix", HUMAN),
            (
                "aaf merge 42 --expect-head 0123456789abcdef0123456789abcdef01234567",
                HUMAN,
            ),
            ("aaf update", HUMAN),
            ("aaf upgrade", HUMAN),
            ("aaf telegram", HUMAN),
            ("aaf ui", HUMAN),
            ("aaf cockpit", HUMAN),
            // Tier-A flag escalations on Tier-B commands: ALWAYS blocked interactively.
            ("aaf run --unsandboxed", HUMAN),
            ("aaf run --dangerously-bypass-permissions", HUMAN),
            ("aaf run --auto-merge", HUMAN),
            ("aaf run --auto", HUMAN),
            ("aaf run --land auto", HUMAN),
            ("aaf run --land=auto", HUMAN),
            ("aaf run --land pr-merge", HUMAN),
            ("aaf run --land=pr-merge", HUMAN),
            ("aaf run --land $MODE", HUMAN),
            ("aaf init --force", HUMAN),
            ("aaf init --commit", HUMAN),
            ("aaf init --auto", HUMAN),
            ("aaf sop run deploy --auto", HUMAN),
            ("aaf sop init --force", HUMAN),
            ("aaf board add --title t --repro-cmd 'echo hi'", HUMAN),
            ("aaf board add --title t --repro-cmd=sh", HUMAN),
            ("aaf board add --title t --repro-arg -c", HUMAN),
            ("aaf skills sync --force", HUMAN),
            ("aaf skills sync --dir /tmp/out", HUMAN),
            ("aaf skills sync --dir=/tmp/out", HUMAN),
            ("aaf skills sync --global", HUMAN),
            ("aaf gc --force", HUMAN),
            ("aaf gc --all", HUMAN),
            // Dynamic subcommands, launchers, AAF_RUN_ID tampering, and Tier-A fallback.
            ("aaf $CMD", HUMAN),
            ("aaf board $CMD T-1", HUMAN),
            ("aaf sop $CMD r1", HUMAN),
            ("aaf proof $CMD", HUMAN),
            ("script -q /dev/null aaf approve", HUMAN),
            ("script -q /dev/null aaf run --unsandboxed", HUMAN),
            ("script -q /dev/null aaf run --land auto", HUMAN),
            ("expect -c 'spawn aaf board steer T-1 focus'", HUMAN),
            ("unset AAF_RUN_ID; aaf run", HUMAN),
            ("env -u AAF_RUN_ID aaf run", HUMAN),
            ("env -i aaf run", HUMAN),
            ("AAF_RUN_ID= aaf gate", HUMAN),
            ("aaf approve 'oops", OPAQUE),
            ("aaf merge 42 'oops", OPAQUE),
            ("aaf board steer T-1 'oops", OPAQUE),
            ("aaf sop send-back r1 'oops", OPAQUE),
            ("aaf run --unsandboxed 'oops", OPAQUE),
            ("aaf init --force 'oops", OPAQUE),
            ("aaf board add --repro-cmd sh 'oops", OPAQUE),
            ("unset AAF_RUN_ID 'oops", OPAQUE),
        ],
    );
}

#[test]
fn worker_blocks_all_tier_a_and_tier_b_and_enforces_trusted_aaf_bins() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let trusted_bin = base.join("trusted/bin/aaf");
    fs::create_dir_all(trusted_bin.parent().unwrap()).unwrap();
    fs::write(&trusted_bin, "#!/bin/sh\n").unwrap();

    let fx = Fixture::new().with_trusted_aaf_bins(vec![trusted_bin.clone()]);
    let trusted = trusted_bin.to_string_lossy().into_owned();

    // Even if `/tmp/.../aaf` is a symlink pointing to the real trusted binary,
    // lexical matching must refuse it (never canonicalize untrusted paths).
    #[cfg(unix)]
    let tmp_symlink = {
        let tmp_dir = tempfile::tempdir_in("/tmp").unwrap();
        let link = tmp_dir.path().join("aaf");
        std::os::unix::fs::symlink(&trusted_bin, &link).unwrap();
        (tmp_dir, link.to_string_lossy().into_owned())
    };

    let mut rows: Vec<(String, Option<&str>)> = vec![
        // All Tier-B blocked inside factory runs.
        ("aaf run".into(), HUMAN),
        ("aaf init".into(), HUMAN),
        ("aaf plan".into(), HUMAN),
        ("aaf gc".into(), HUMAN),
        ("aaf skills sync".into(), HUMAN),
        ("aaf sop run deploy".into(), HUMAN),
        ("aaf sop init".into(), HUMAN),
        ("aaf board add --title t --accept a".into(), HUMAN),
        ("aaf board ready T-1".into(), HUMAN),
        ("aaf board split T-1".into(), HUMAN),
        ("aaf board dep T-2 --after T-1".into(), HUMAN),
        ("aaf board import-issues".into(), HUMAN),
        ("aaf board sync-github".into(), HUMAN),
        ("aaf board rework T-1".into(), HUMAN),
        // `aaf merge --dry-run` is ALSO blocked inside factory runs.
        (
            "aaf merge 42 --expect-head 0123456789abcdef0123456789abcdef01234567 --dry-run".into(),
            HUMAN,
        ),
        // Trusted absolute path in `trusted_aaf_bins` opens a valid slot,
        // even when `PATH` is exported earlier on the line.
        (format!("{trusted} slot -- ./gradlew assembleDebug"), ALLOW),
        (
            format!("export PATH=/opt/jdk/bin:$PATH; {trusted} slot -- ./gradlew assembleDebug"),
            ALLOW,
        ),
        (
            format!("{trusted} slot -- sh -c './gradlew a && ./gradlew b'"),
            ALLOW,
        ),
        // Untrusted paths and bare PATH-rebound `aaf` stay blocked.
        ("/tmp/aaf slot -- ./gradlew assembleDebug".into(), HEAVY),
        (
            "export PATH=/tmp/evil:$PATH; aaf slot -- ./gradlew assembleDebug".into(),
            HEAVY,
        ),
        // A trusted slot still does not cover a subsequent bare heavy command.
        (format!("{trusted} slot -- ./gradlew a; ./gradlew b"), HEAVY),
    ];

    #[cfg(unix)]
    rows.push((
        format!("{} slot -- ./gradlew assembleDebug", tmp_symlink.1),
        HEAVY,
    ));

    let borrowed: Vec<(&str, Option<&str>)> =
        rows.iter().map(|(cmd, exp)| (cmd.as_str(), *exp)).collect();
    run_corpus(&fx, &borrowed);
}

#[test]
fn env_templates_allowed_but_symlink_to_secret_env_still_denied() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=real\n");
    fx.write(".env.example", "SECRET=\n");
    fx.write(".env.sample", "SECRET=\n");
    fx.write(".env.template", "SECRET=\n");

    run_file_corpus(
        &fx,
        &[
            ("Write", ".env.example", ALLOW),
            ("Edit", ".env.sample", ALLOW),
            ("MultiEdit", ".env.template", ALLOW),
            ("Read", ".env.example", ALLOW),
            ("Write", ".env", DELETE),
            ("Write", ".env.local", DELETE),
            ("Write", ".env.production", DELETE),
        ],
    );
    run_corpus(
        &fx,
        &[
            ("echo 'FOO=' > .env.example", ALLOW),
            ("rm .env.sample", ALLOW),
            ("cp .env.template .env.example", ALLOW),
            ("find . -name '.env.example' -delete", ALLOW),
            ("echo 'FOO=1' > .env", DELETE),
            ("rm .env", DELETE),
            ("find . -name '.env*' -delete", DELETE),
        ],
    );

    #[cfg(unix)]
    {
        fs::remove_file(fx.root.join(".env.example")).unwrap();
        std::os::unix::fs::symlink(fx.root.join(".env"), fx.root.join(".env.example")).unwrap();
        run_file_corpus(
            &fx,
            &[
                ("Write", ".env.example", DELETE),
                ("Edit", ".env.example", DELETE),
                ("Read", ".env.example", SECRET),
            ],
        );
        run_corpus(
            &fx,
            &[
                ("echo 'FOO=1' > .env.example", DELETE),
                ("rm .env.example", DELETE),
                ("cat .env.example", SECRET),
            ],
        );
    }
}

#[test]
fn adb_swipe_allowed_and_wipe_commands_still_blocked() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            // False positives fixed by HOOK-09.
            ("adb shell input swipe 500 1500 500 500", ALLOW),
            (
                "adb shell input touchscreen swipe 100 200 300 400 250",
                ALLOW,
            ),
            (
                "adb shell am instrument -w -e class com.app.SwipeTest com.app.test/androidx.test.runner.AndroidJUnitRunner",
                ALLOW,
            ),
            ("adb shell input swipe 1 2 3 4 'oops", ALLOW),
            // Real wipe commands remain blocked.
            ("adb shell wipe data", ADB),
            ("adb shell /sbin/wipe all", ADB),
            ("adb shell wipefs -a /dev/block/sda", ADB),
            ("adb shell recovery --wipe_data", ADB),
            ("adb shell recovery --wipe_cache", ADB),
            ("adb shell factory-wipe", ADB),
            ("adb shell data_wipe", ADB),
            ("adb shell wipe-data", ADB),
            ("adb shell recovery --wipe_data 'oops", OPAQUE),
        ],
    );
}

#[test]
fn gradle_local_publish_tasks_exempt_from_store_guard() {
    let worker = Fixture::new();
    run_corpus(
        &worker,
        &[
            ("aaf slot -- ./gradlew publishToMavenLocal", ALLOW),
            (
                "aaf slot -- ./gradlew :sdk:publishReleasePublicationToMavenLocal",
                ALLOW,
            ),
            (
                "aaf slot -- ./gradlew publishAllPublicationsToLocalRepository",
                ALLOW,
            ),
            (
                "aaf slot -- ./gradlew :lib:publishLocalPublicationToCustomDir",
                ALLOW,
            ),
            // Remote store/registry publish tasks remain blocked.
            ("aaf slot -- ./gradlew publishReleaseBundle", STORE),
            (
                "aaf slot -- ./gradlew publishToMavenLocal publishReleaseBundle",
                STORE,
            ),
            ("aaf slot -- ./gradlew promoteArtifact --track beta", STORE),
        ],
    );
    let interactive = Fixture::new().interactive();
    run_corpus(
        &interactive,
        &[
            ("./gradlew publishToMavenLocal", ALLOW),
            ("./gradlew publishReleaseBundle", STORE),
        ],
    );
}

#[test]
fn data_guard_exempts_static_search_patterns_while_blocking_bypasses() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            // Static search/filter patterns allowed (HOOK-11).
            ("rg receipt.key crates/", ALLOW),
            ("rg AAF_DATA_DIR crates/", ALLOW),
            ("rg -i --fixed-strings receipt.key crates/", ALLOW),
            ("rg -g '*.rs' AAF_DATA_DIR crates/", ALLOW),
            ("rg -e receipt.key -e AAF_DATA_DIR crates/", ALLOW),
            ("rg --regexp=receipt.key crates/", ALLOW),
            ("grep -rn receipt.key crates/", ALLOW),
            ("grep -rn 'AAF_DATA_DIR' src/", ALLOW),
            ("grep -ieAAF_DATA_DIR src/", ALLOW),
            ("egrep 'receipt\\.key|AAF_DATA_DIR' src/", ALLOW),
            ("fgrep receipt.key src/", ALLOW),
            ("git grep receipt.key", ALLOW),
            ("git grep -n AAF_DATA_DIR crates/", ALLOW),
            ("git grep -e receipt.key crates/", ALLOW),
            ("git log -S receipt.key --oneline", ALLOW),
            ("git log -G AAF_DATA_DIR -n 5", ALLOW),
            ("git log --grep=AAF_DATA_DIR --oneline", ALLOW),
            ("cargo test -p aaf-hook AAF_DATA_DIR", ALLOW),
            ("cargo test -- receipt.key --nocapture", ALLOW),
            // Bypasses MUST remain blocked.
            ("rg . $AAF_DATA_DIR", DATA),
            ("rg foo ~/.local/share/autofactory", DATA),
            ("rg foo {data}", DATA),
            ("rg -e foo {data}/aaf.db", DATA),
            ("rg -f receipt.key crates/", DATA),
            ("rg --file=receipt.key crates/", DATA),
            ("rg -f $AAF_DATA_DIR/receipt.key crates/", DATA),
            ("rg -f /dev/null receipt.key", DATA),
            ("rg \"$AAF_DATA_DIR\" crates/", DATA),
            ("rg -e \"$AAF_DATA_DIR\" crates/", DATA),
            ("rg receipt.key < $AAF_DATA_DIR/receipt.key", DATA),
            ("rg receipt.key > {data}/out.txt", DATA),
            ("grep -f receipt.key src/", DATA),
            ("grep -freceipt.key src/", DATA),
            ("grep foo $AAF_DATA_DIR/aaf.db", DATA),
            ("git grep -f receipt.key", DATA),
            ("git grep foo -- $AAF_DATA_DIR/receipt.key", DATA),
            ("git log -- $AAF_DATA_DIR/receipt.key", DATA),
            ("git log -S \"$AAF_DATA_DIR\"", DATA),
            ("cargo test --manifest-path $AAF_DATA_DIR/Cargo.toml", DATA),
            ("cargo test -- --logfile $AAF_DATA_DIR/out.log", DATA),
            ("cargo test \"$AAF_DATA_DIR\"", DATA),
        ],
    );
}

#[test]
fn cargo_run_only_triggers_for_aaf_crate_or_workspace() {
    // Non-aaf Rust repository: `cargo run -- init` / `approve` allowed.
    let other_repo = Fixture::new();
    other_repo.write("Cargo.toml", "[package]\nname = \"my-other-cli\"\n");
    run_corpus(
        &other_repo,
        &[
            ("cargo run -- init", ALLOW),
            ("cargo run -- run", ALLOW),
            ("cargo run -- approve", ALLOW),
            ("cargo run -- board unblock T-1", ALLOW),
            // Explicit `-p aaf-cli` / `--bin aaf` still triggers.
            ("cargo run -p aaf-cli -- approve", HUMAN),
            ("cargo run --bin aaf -- init", HUMAN),
        ],
    );

    // Interactive mode in an aaf workspace: respects Tier-A vs Tier-B.
    let aaf_interactive = Fixture::new().interactive();
    aaf_interactive.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/aaf-cli\"]\n",
    );
    run_corpus(
        &aaf_interactive,
        &[
            ("cargo run -- init", ALLOW),
            ("cargo run -- run", ALLOW),
            ("cargo run -- approve", HUMAN),
            ("cargo run -- init --force", HUMAN),
            ("cargo run -- run --unsandboxed", HUMAN),
            ("cargo run -- board steer T-1 focus", HUMAN),
        ],
    );
}

#[test]
fn worker_fix_messages_are_agent_actionable() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    let cases = [
        ("aaf approve", "human_action_guard"),
        ("rm .env", "delete_guard"),
        ("adb reboot bootloader", "adb_guard"),
        ("echo x > .git/hooks/pre-commit", "git_guard"),
        ("cat .env", "secret_guard"),
        ("fastlane deliver", "store_submit_guard"),
    ];
    for (cmd, expected_rule) in cases {
        let Decision::Deny(denial) = fx.bash(cmd) else {
            panic!("expected denial for `{cmd}`");
        };
        assert_eq!(denial.rule, expected_rule, "cmd={cmd}");
        let lower = denial.fix.to_ascii_lowercase();
        assert!(
            !lower.contains("ask the user"),
            "worker fix should not tell agent to ask the user: cmd={cmd}, fix={}",
            denial.fix
        );
        assert!(
            !lower.contains("by a human"),
            "worker fix should not tell agent it is changed by a human: cmd={cmd}, fix={}",
            denial.fix
        );
        assert_ne!(
            denial.fix, "aaf gate --release",
            "worker fix should not be bare unrunnable `aaf gate --release`"
        );
    }
}

#[test]
fn binary_respects_aaf_run_id_for_interactive_vs_worker_mode() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    for sub in ["data", "config", "home", "work"] {
        fs::create_dir(base.join(sub)).unwrap();
    }
    let run_bin = |command: &str, run_id: Option<&str>| -> i32 {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_aaf-hook"));
        cmd.env_clear()
            .env("AAF_NO_BUILTIN_DEFAULTS", "1")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("AAF_DATA_DIR", base.join("data"))
            .env("AAF_CONFIG_DIR", base.join("config"))
            .env("HOME", base.join("home"))
            .current_dir(base.join("work"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(id) = run_id {
            cmd.env("AAF_RUN_ID", id);
        }
        let mut child = cmd.spawn().unwrap();
        let payload = json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": command},
            "cwd": base.join("work"),
        });
        let _ = child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes());
        child.wait().unwrap().code().unwrap()
    };

    // Interactive (`AAF_RUN_ID` unset or empty): Tier-B & direct heavy builds allowed; Tier-A & escalations denied.
    assert_eq!(run_bin("aaf run", None), 0);
    assert_eq!(run_bin("aaf run", Some("")), 0);
    assert_eq!(run_bin("aaf board add --title t --accept a", None), 0);
    assert_eq!(run_bin("./gradlew test", None), 0);
    assert_eq!(run_bin("aaf approve", None), 2);
    assert_eq!(run_bin("aaf run --unsandboxed", None), 2);
    assert_eq!(run_bin("aaf merge 1 --expect-head abc", None), 2);

    // Worker (`AAF_RUN_ID` non-empty): Tier-B & bare heavy builds denied.
    assert_eq!(run_bin("aaf run", Some("run-1")), 2);
    assert_eq!(
        run_bin("aaf board add --title t --accept a", Some("run-1")),
        2
    );
    assert_eq!(run_bin("./gradlew test", Some("run-1")), 2);
    assert_eq!(run_bin("aaf slot -- ./gradlew test", Some("run-1")), 0);
}
