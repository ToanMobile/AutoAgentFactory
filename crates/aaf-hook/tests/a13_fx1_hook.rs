#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit-13 lane 6 (hook) findings H6-1..H6-7.

mod common;

use std::fs;
use std::path::Path;

use aaf_hook::{Decision, HookEnv, decide};
use common::{Fixture, rule, run_corpus};
use serde_json::json;

const ALLOW: Option<&str> = None;
const ADB: Option<&str> = Some("adb_guard");
const DATA: Option<&str> = Some("data_dir_guard");
const DELETE: Option<&str> = Some("delete_guard");
const HEAVY: Option<&str> = Some("heavy_cmd_guard");
const INTERP: Option<&str> = Some("interp_guard");
const PR_MERGE: Option<&str> = Some("pr_merge_guard");

/// H6-1: a quoted heredoc is the interpreter's program only when the
/// interpreter reads its program from stdin; otherwise it is stdin for a
/// program that can hand it to a shell.
#[test]
fn h6_1_quoted_heredoc_to_interpreter_is_judged() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("perl -e 'exec \"sh\"' <<'EOF'\nrm -rf ~\nEOF", DELETE),
            (
                "python3 -c 'import os; os.execvp(\"sh\", [\"sh\"])' <<'EOF'\nrm -rf ~\nEOF",
                DELETE,
            ),
            ("cat <<'EOF' | perl -e 'exec \"sh\"'\nrm -rf ~\nEOF", DELETE),
            ("python3 script.py <<'EOF'\nrm -rf ~\nEOF", DELETE),
            (
                "python3 - <<'EOF'\nimport shutil\nshutil.copy('k', '{data}/receipt.key')\nEOF",
                DATA,
            ),
            (
                "python3 <<'EOF'\nimport shutil\nshutil.copy('k', '{data}/x')\nEOF",
                DATA,
            ),
            // The heredoc is the program: plain Python text stays allowed.
            ("python3 - <<'EOF'\nprint(\"don't worry\")\nEOF", ALLOW),
            ("python3 <<'EOF'\nprint(\"don't worry\")\nEOF", ALLOW),
            ("cat <<'EOF' | python3\nprint(\"don't worry\")\nEOF", ALLOW),
            ("perl <<'EOF'\nprint \"don't worry\\n\";\nEOF", ALLOW),
        ],
    );
}

/// A repository lookup like git's: only paths inside the fixture repo have one.
fn real_env_decide(fx: &Fixture, command: &str) -> Decision {
    let root = fx.root.clone();
    let lookup = move |dir: &Path| dir.starts_with(&root).then(|| root.clone());
    let config = |_: &Path| Some(String::new());
    let env = HookEnv {
        repo_root: &lookup,
        git_config: &config,
        data_dir: Some(fx.data.clone()),
        config_dir: Some(fx.config.clone()),
        home: Some(fx.home.clone()),
        cwd: None,
    };
    let payload = json!({
        "tool_name": "Bash",
        "tool_input": {"command": fx.fill(command)},
        "cwd": fx.root,
    });
    decide(payload.to_string().as_bytes(), &env)
}

/// H6-2: leaving the repository with `cd`/`env -C` does not take it out of
/// the guards' reach.
#[test]
fn h6_2_cd_out_of_repo_still_protects_it() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write(".claude/settings.json", "{}\n");
    fs::create_dir_all(fx.root.join("build")).unwrap();
    let rows: &[(&str, Option<&str>)] = &[
        ("cd /tmp && rm -rf {root}", DELETE),
        ("cd /tmp; rm -rf {root}", DELETE),
        ("env -C /tmp rm -rf {root}", DELETE),
        ("cd /tmp && rm -rf {root}/", DELETE),
        ("cd /tmp && rm -rf {root}/..", DELETE),
        ("cd /tmp && rm -rf {root}/.claude", DELETE),
        (
            "cd /tmp && echo '{}' > {root}/.claude/settings.json",
            DELETE,
        ),
        ("cd /tmp && rm {root}/.env", DELETE),
        ("env -C /tmp rm {root}/.env", DELETE),
        ("cd /tmp && mv {root}/.env /tmp/x", DELETE),
        // Still allowed.
        ("cd /tmp && rm -rf {root}/build", ALLOW),
        ("cd /tmp && rm -rf /tmp/aaf-fx1-scratch", ALLOW),
        ("cd /tmp/x && rm -rf build", ALLOW),
        ("rm -rf build", ALLOW),
        ("cd build && rm -rf out", ALLOW),
    ];
    let mut wrong = Vec::new();
    for (command, expected) in rows {
        let got = real_env_decide(&fx, command);
        if rule(&got) != *expected {
            wrong.push(format!("{command:?}: expected {expected:?}, got {got:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} rows wrong:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
    // The fixture's lookup (a repo everywhere) agrees on the root itself.
    run_corpus(&fx, &[("cd /tmp && rm -rf {root}", DELETE)]);
}

/// H6-3: a `cd` only moves the commands after it for sure when it is joined
/// to them by `&&` and runs in the current shell.
#[test]
fn h6_3_uncertain_cd_judges_both_directories() {
    let fx = Fixture::new();
    fs::create_dir_all(fx.root.join("sub")).unwrap();
    run_corpus(
        &fx,
        &[
            ("cd /tmp/nope/x; rm -rf ../*", DELETE),
            ("cd /tmp/nope/x\nrm -rf ../*", DELETE),
            ("cd /tmp/a/b | true; rm -rf ../*", DELETE),
            ("cd /tmp/a/b & rm -rf ../*", DELETE),
            ("cd /tmp/a/b || true; rm -rf ../*", DELETE),
            ("true || cd /tmp/a/b && rm -rf ../*", DELETE),
            ("echo | cd /tmp/a/b && rm -rf ../*", DELETE),
            ("cd /tmp/a/b; cd .. && rm -rf ../*", DELETE),
            // Whichever way the first `cd` went, the second one decides.
            ("cd /tmp/nope/x; cd /tmp/c/d && rm -rf ../*", ALLOW),
            // Sure moves are still followed.
            ("cd /tmp/a/b && rm -rf ../*", ALLOW),
            ("cd /tmp/a/b && cd /tmp/c/d && rm -rf ../*", ALLOW),
            ("(cd /tmp/a/b && rm -rf ../*)", ALLOW),
            ("cd sub && cargo test", ALLOW),
            ("cd /tmp/x && rm -rf build", ALLOW),
            ("cd /tmp/x; rm -rf build", ALLOW),
            ("cd sub; ls ..", ALLOW),
        ],
    );
}

/// H6-4: interpreter one-liners that write git hooks or factory settings.
#[test]
fn h6_4_interpreter_writes_to_guarded_files() {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}\n");
    fs::create_dir_all(fx.root.join(".git/hooks")).unwrap();
    run_corpus(
        &fx,
        &[
            (
                "python3 -c \"open('.git/hooks/pre-commit','w').write('x')\"",
                INTERP,
            ),
            (
                "python3 -c \"import shutil; shutil.copy('x', '.git/hooks/pre-push')\"",
                INTERP,
            ),
            (
                "node -e \"require('fs').writeFileSync('.claude/settings.json','{}')\"",
                INTERP,
            ),
            (
                "ruby -e \"File.write('.claude/settings.json', '{}')\"",
                INTERP,
            ),
            (
                "perl -e 'open(my $f, \">\", \".git/hooks/pre-commit\")'",
                INTERP,
            ),
            ("perl -e 'open(F, \">.git/hooks/pre-commit\")'", INTERP),
            (
                "python3 - <<'EOF'\nfrom pathlib import Path\nPath('.claude/settings.json').write_text('{}')\nEOF",
                INTERP,
            ),
            // Reads and ordinary writes stay allowed.
            (
                "python3 -c \"print(open('.claude/settings.json').read())\"",
                ALLOW,
            ),
            ("python3 -c \"print(open('.git/config').read())\"", ALLOW),
            ("python3 -c \"open('out.txt','w').write('x')\"", ALLOW),
            (
                "node -e \"require('fs').writeFileSync('dist/a.js','x')\"",
                ALLOW,
            ),
        ],
    );
}

/// H6-5: merges through a gh alias or a raw HTTP client.
#[test]
fn h6_5_alias_and_http_client_merges() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("gh alias set mm 'pr merge'", PR_MERGE),
            ("gh alias set --shell mm 'gh pr merge \"$1\"'", PR_MERGE),
            ("gh alias import - <<'EOF'\nmm: pr merge\nEOF", PR_MERGE),
            ("gh alias import aliases.yml", PR_MERGE),
            (
                "curl -X PUT -H 'Authorization: token x' https://api.github.com/repos/o/r/pulls/12/merge",
                PR_MERGE,
            ),
            (
                "wget --method=PUT https://api.github.com/repos/o/r/pulls/12/merge",
                PR_MERGE,
            ),
            (
                "curl -X POST https://api.github.com/graphql -d '{\"query\":\"mutation{mergePullRequest(input:{})}\"}'",
                PR_MERGE,
            ),
            // Still allowed.
            ("gh alias set co 'pr checkout'", ALLOW),
            ("gh alias list", ALLOW),
            ("curl https://api.github.com/repos/o/r/pulls/12", ALLOW),
            (
                "curl https://api.github.com/repos/o/r/pulls/12/files",
                ALLOW,
            ),
        ],
    );
}

/// H6-6: a heavy build behind a launcher is still a heavy build.
#[test]
fn h6_6_heavy_build_behind_launcher() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("xcrun xcodebuild -scheme App build", HEAVY),
            ("xcrun --sdk iphoneos xcodebuild build", HEAVY),
            ("xcrun -sdk iphoneos -r xcodebuild build", HEAVY),
            ("mise exec -- gradle build", HEAVY),
            ("bundle exec xcodebuild build", HEAVY),
            ("npx gradle build", HEAVY),
            // Still allowed.
            ("xcrun xcodebuild -version", ALLOW),
            ("xcrun -f xcodebuild", ALLOW),
            ("xcrun --find xcodebuild", ALLOW),
            ("xcrun simctl list", ALLOW),
            ("aaf slot -- xcrun xcodebuild -scheme App build", ALLOW),
        ],
    );
}

/// H6-7: device-side recursive deletes whose target is not a deep literal
/// absolute path.
#[test]
fn h6_7_adb_shell_rm_relative_or_dynamic_targets() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("adb shell rm -rf sdcard", ADB),
            ("adb shell rm -rf ./*", ADB),
            ("adb shell 'cd /sdcard/x && rm -rf ../..'", ADB),
            ("adb shell 'rm -rf $EXTERNAL_STORAGE'", ADB),
            ("adb shell 'rm -rf ${EXTERNAL_STORAGE}/'", ADB),
            ("adb shell rm -rf /sdcard/../data", ADB),
            ("adb shell rm -rf '~'", ADB),
            // Deep absolute paths stay allowed.
            ("adb shell rm -rf /sdcard/Android/data/x/cache", ALLOW),
            ("adb shell rm -f /sdcard/Download/a.txt", ALLOW),
            ("adb shell ls -R sdcard", ALLOW),
        ],
    );
}

/// H6-8: `find` deletes and `rsync --delete` are held to the same
/// containment as `rm -r`: inside the repository, `/tmp` or `$TMPDIR` only.
#[test]
fn h6_8_find_and_rsync_deletes_outside_repo() {
    let fx = Fixture::new();
    fs::create_dir_all(fx.root.join("build")).unwrap();
    fs::create_dir_all(fx.home.join("Documents")).unwrap();
    fs::create_dir_all(fx.home.join("other-repo")).unwrap();
    run_corpus(
        &fx,
        &[
            ("find ../init1 -delete", DELETE),
            ("find ../init1 -exec rm -rf {} +", DELETE),
            ("find ../init1 -print0 | xargs -0 rm -rf", DELETE),
            ("find ~/Documents -delete", DELETE),
            ("find {home}/other-repo -delete", DELETE),
            ("find / -name '*.rs' -delete", DELETE),
            ("find .. -name '*.rs' -delete", DELETE),
            ("find .. -name '*.rs' | xargs rm", DELETE),
            ("rsync -a --delete /var/empty/ ../init1/", DELETE),
            ("rsync -a --delete-after build/ {home}/other-repo/", DELETE),
            ("rsync -a --remove-source-files ../init1/ build/", DELETE),
            // Still allowed.
            ("find . -name '*.tmp' -delete", ALLOW),
            ("find build -type f -name '*.log' -delete", ALLOW),
            ("find /tmp/elsewhere -delete", ALLOW),
            ("find /tmp/elsewhere -name .env | xargs rm", ALLOW),
            ("find .. -name '*.rs'", ALLOW),
            ("rsync -a --delete build/ /tmp/out/", ALLOW),
            ("rsync -a --delete /tmp/src/ build/", ALLOW),
            ("rsync -a build/ ../init1/", ALLOW),
        ],
    );
}
