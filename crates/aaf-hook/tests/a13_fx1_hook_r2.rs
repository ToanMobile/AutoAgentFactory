#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit-13 round-2 lane 6 (hook) findings R2-H6-1..7.

mod common;

use std::fs;
use std::path::Path;

use aaf_hook::{Decision, HookEnv, decide};
use common::{Fixture, rule, run_corpus};
use serde_json::json;

const ALLOW: Option<&str> = None;
const ADB: Option<&str> = Some("adb_guard");
const DELETE: Option<&str> = Some("delete_guard");
const GIT: Option<&str> = Some("git_guard");
const HEAVY: Option<&str> = Some("heavy_cmd_guard");
const INTERP: Option<&str> = Some("interp_guard");
const PR_MERGE: Option<&str> = Some("pr_merge_guard");

/// R2-H6-1: a `cd` joined by `&&` moves only the commands of its own
/// `&&` chain; a `cd` run by another process, a `cd` function, or one
/// inside a heredoc handed to a substitution moves nothing for sure.
#[test]
fn r2_h6_1_sure_cd_holds_only_for_its_and_chain() {
    let fx = Fixture::new();
    fs::create_dir_all(fx.root.join("sub")).unwrap();
    run_corpus(
        &fx,
        &[
            ("cd /tmp/nope/x && :; rm -rf ../*", DELETE),
            ("cd /tmp/nope/x && :\nrm -rf ../*", DELETE),
            ("cd /tmp/nope/x && true || rm -rf ../*", DELETE),
            ("cd /tmp/nope/x && :; rm -rf ../sibling", DELETE),
            ("cd /tmp/nope/x && cd /tmp/c/d && :; rm -rf ../*", DELETE),
            ("env cd /tmp/a/b && rm -rf ../*", DELETE),
            ("nice cd /tmp/a/b && rm -rf ../*", DELETE),
            ("timeout 5 cd /tmp/a/b && rm -rf ../*", DELETE),
            ("nohup cd /tmp/a/b && rm -rf ../*", DELETE),
            ("xargs cd /tmp/a/b && rm -rf ../*", DELETE),
            ("cd() { :; }; cd /tmp/a/b && rm -rf ../*", DELETE),
            ("function cd { true; }; cd /tmp/a/b && rm -rf ../*", DELETE),
            (
                "echo \"$(cat <<'EOF'\ncd /tmp/a/b && :\nEOF\n)\" && rm -rf ../*",
                DELETE,
            ),
            ("cd /tmp/nope/x; rm -rf ../*", DELETE),
            // Sure moves are still followed inside their chain.
            ("cd sub && cargo test", ALLOW),
            ("cd /tmp/x && rm -rf build", ALLOW),
            ("cd /tmp/a/b && rm -rf ../*", ALLOW),
            ("cd /tmp/a/b && cd /tmp/c/d && rm -rf ../*", ALLOW),
            ("command cd /tmp/a/b && rm -rf ../*", ALLOW),
            ("cd /tmp/a/b && ls && rm -rf ../*", ALLOW),
            (
                "cd /tmp/a/b && echo \"$(cat <<'EOF'\nhi\nEOF\n)\" && rm -rf ../*",
                ALLOW,
            ),
            ("cd /tmp/a/b && echo $(pwd) && rm -rf ../*", ALLOW),
            ("(cd /tmp/a/b && rm -rf ../*); ls", ALLOW),
            ("cd sub && npm test && cd .. && ls", ALLOW),
            ("cd sub && make; cd ..", ALLOW),
        ],
    );
}

/// R2-H6-2: a launcher (`xcrun`, `arch`, `bundle exec`, `mise exec`,
/// `npm exec`, `npx`) is stripped like any wrapper, so every guard judges
/// the command it runs; `npx -c` runs its value as a shell line.
#[test]
fn r2_h6_2_launchers_are_judged_by_every_guard() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("xcrun rm -rf ../sibling", DELETE),
            ("bundle exec rm -rf ../sibling", DELETE),
            ("arch -arm64 rm -rf ../sibling", DELETE),
            ("arch -arch x86_64 rm -rf ../sibling", DELETE),
            ("npx -c 'rm -rf ../sibling'", DELETE),
            ("npm exec -- rm -rf ../sibling", DELETE),
            ("pnpm dlx rm -rf ../sibling", DELETE),
            ("rbenv exec rm -rf ../sibling", DELETE),
            ("mise exec -- sh -c 'git push --force origin main'", GIT),
            ("npx -c 'gradle build'", HEAVY),
            ("bundle exec bash -c './gradlew assembleDebug'", HEAVY),
            ("mise exec -- sh -c 'xcodebuild build'", HEAVY),
            ("arch -arm64 xcodebuild build", HEAVY),
            ("arch -arm64 xcodebuild -scheme A build", HEAVY),
            ("arch -x86_64 ./gradlew build", HEAVY),
            ("npm exec -- gradle build", HEAVY),
            ("npm x gradle build", HEAVY),
            // A launcher's own directory option moves its command.
            ("pnpm -C .. exec rm -rf repo", DELETE),
            ("pnpm --dir=.. exec rm -rf repo", DELETE),
            ("mise exec -C .. -- rm -rf repo", DELETE),
            ("yarn --cwd .. exec rm -rf repo", DELETE),
            // Still allowed.
            ("npx prettier --check .", ALLOW),
            ("bundle exec rspec", ALLOW),
            ("xcrun -f clang", ALLOW),
            ("xcrun simctl list", ALLOW),
            ("arch", ALLOW),
            ("npm install", ALLOW),
            ("bundle install", ALLOW),
            ("npm exec -- eslint .", ALLOW),
            ("aaf slot -- npx gradle build", ALLOW),
            ("yarn run build", ALLOW),
            ("pnpm run lint", ALLOW),
            ("npx tsc --noEmit", ALLOW),
            ("mise exec -- cargo test", ALLOW),
            ("xcrun simctl boot 'iPhone 15'", ALLOW),
            ("bundle exec rm -rf build", ALLOW),
        ],
    );
}

/// R2-H6-3: `find`'s leading global options do not hide its search roots,
/// a root given with `-f` is one, and a root filled in at run time is
/// refused.
#[test]
fn r2_h6_3_find_roots_after_global_options() {
    let fx = Fixture::new();
    fs::create_dir_all(fx.root.join("build")).unwrap();
    fs::create_dir_all(fx.home.join("Documents")).unwrap();
    run_corpus(
        &fx,
        &[
            ("find -sx ~/Documents -name '*.pdf' -delete", DELETE),
            ("find -xL ../sibling -name '*.rs' | xargs rm", DELETE),
            ("find -O3 ../sibling -name '*.rs' -delete", DELETE),
            ("find -D stat ../sibling -name '*.rs' -delete", DELETE),
            ("find -f ../sibling -name '*.rs' -delete", DELETE),
            ("find \"$(dirname $PWD)\" -name '*.rs' -delete", DELETE),
            // Still allowed.
            ("find -L build -name '*.o' -delete", ALLOW),
            ("find -sx build -name '*.o' -delete", ALLOW),
            ("find -O3 . -name '*.tmp' -delete", ALLOW),
            ("find -x ~/Documents -name '*.pdf'", ALLOW),
        ],
    );
}

/// R2-E10-1: `find` rooted at a temp directory itself only deletes inside
/// it, like `rm -rf /tmp/<name>`; other roots keep `rm -r`'s containment.
#[test]
fn r2_e10_1_find_rooted_at_temp_dir() {
    let fx = Fixture::new();
    fs::create_dir_all(fx.home.join("Documents")).unwrap();
    run_corpus(
        &fx,
        &[
            (
                "find /tmp -maxdepth 1 -name 'aaf-e2e-*' -mtime +1 -delete",
                ALLOW,
            ),
            ("find /tmp -name 'aaf-e2e-*' -exec rm -rf {} +", ALLOW),
            ("find /tmp/ -name x -delete", ALLOW),
            ("find /private/tmp -name 'aaf-*' -delete", ALLOW),
            ("find /tmp -name 'aaf-*' | xargs rm -rf", ALLOW),
            // Still denied.
            ("find / -name '*.rs' -delete", DELETE),
            ("find ~/Documents -delete", DELETE),
            ("find .. -name '*.rs' -delete", DELETE),
            ("find /tmp/.. -name '*.rs' -delete", DELETE),
            ("find /private -name '*.rs' -delete", DELETE),
            // The fixture's repository and home live in `$TMPDIR`.
            ("find \"$TMPDIR\" -name 'aaf-*' -delete", DELETE),
        ],
    );
    // A repository outside every temp directory, as on a real machine.
    let base = Path::new(env!("CARGO_TARGET_TMPDIR")).join("a13-fx1-r2-e10");
    let root = base.join("repo");
    let home = base.join("home");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&home).unwrap();
    let lookup = |_: &Path| Some(root.clone());
    let config = |_: &Path| Some(String::new());
    let env = HookEnv {
        repo_root: &lookup,
        git_config: &config,
        data_dir: Some(base.join("data")),
        config_dir: Some(base.join("config")),
        home: Some(home.clone()),
        cwd: None,
    };
    for (command, expected) in [
        ("find \"$TMPDIR\" -name 'aaf-*' -delete", ALLOW),
        ("find /tmp -maxdepth 1 -name 'aaf-e2e-*' -delete", ALLOW),
        ("find \"$TMPDIR/..\" -name 'aaf-*' -delete", DELETE),
        ("find .. -name '*.rs' -delete", DELETE),
    ] {
        let payload = json!({
            "tool_name": "Bash",
            "tool_input": {"command": command},
            "cwd": root,
        });
        let got = decide(payload.to_string().as_bytes(), &env);
        assert_eq!(rule(&got), expected, "{command}: {got:?}");
    }
}

/// R2-H6-4: every spelling rsync accepts for source removal counts.
#[test]
fn r2_h6_4_rsync_source_removal_spellings() {
    let fx = Fixture::new();
    fs::create_dir_all(fx.root.join("build")).unwrap();
    run_corpus(
        &fx,
        &[
            ("rsync -a --remove-sent-files ../sibling/ build/", DELETE),
            ("rsync -a --remove-sou ../sibling/ build/", DELETE),
            ("rsync -a --remove-source-files ../sibling/ build/", DELETE),
            // Still allowed.
            ("rsync -a --remove-source-files /tmp/in/ build/", ALLOW),
            ("rsync -a ../sibling/ build/", ALLOW),
        ],
    );
}

/// R2-H6-5: gh aliases that can merge, merge endpoints spelled with dot
/// segments, and fewer false alarms on text that only mentions a merge.
#[test]
fn r2_h6_5_gh_alias_and_endpoint_spellings() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("gh alias set mm 'pr mer\"\"ge'", PR_MERGE),
            ("gh alias set mm \"pr 'merge'\"", PR_MERGE),
            ("gh alias set --shell mm 'gh pr list'", PR_MERGE),
            ("gh alias set -s mm 'gh pr list'", PR_MERGE),
            ("gh alias set mm '!gh pr m\"er\"ge'", PR_MERGE),
            ("gh alias import - <<'EOF'\nmm: pr m\"er\"ge\nEOF", PR_MERGE),
            ("gh alias import aliases.yml", PR_MERGE),
            ("gh alias set p 'pr $1'", ALLOW),
            ("gh p merge 12", PR_MERGE),
            (
                "curl -X PUT https://api.github.com/repos/o/r/pulls/12/./merge",
                PR_MERGE,
            ),
            (
                "curl -X PUT https://api.github.com/repos/o/r/pulls/12/x/../merge",
                PR_MERGE,
            ),
            ("gh api -X PUT repos/o/r/pulls/12/./merge", PR_MERGE),
            (
                "python3 -c \"import requests; requests.put('https://api.github.com/repos/o/r/pulls/12/merge')\"",
                PR_MERGE,
            ),
            // Round-1 rows keep their verdicts.
            ("gh alias set mm 'pr merge'", PR_MERGE),
            ("gh alias set --shell mm 'gh pr merge \"$1\"'", PR_MERGE),
            ("gh alias import - <<'EOF'\nmm: pr merge\nEOF", PR_MERGE),
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
            // Allowed: a search qualifier, and text that only names the URL.
            ("gh alias set prs 'pr list --search is:merged'", ALLOW),
            ("gh alias set co 'pr checkout'", ALLOW),
            ("echo https://github.com/o/r/pulls/12/merge", ALLOW),
            ("git log | grep 'repos/o/r/pulls/1/merge'", ALLOW),
            ("gh pr list --search is:merged", ALLOW),
            ("gh issue list", ALLOW),
            ("gh copilot suggest 'merge two branches'", ALLOW),
            ("gh extension list", ALLOW),
            ("gh pr view 12 --json mergeable", ALLOW),
            (
                "curl https://api.github.com/repos/o/r/pulls/12/files",
                ALLOW,
            ),
        ],
    );
}

fn reason(decision: &Decision) -> String {
    match decision {
        Decision::Allow => String::new(),
        Decision::Deny(denial) => denial.reason.to_string(),
    }
}

/// R2-H6-6: only the paths a script writes to are held against protected
/// and git files; a file it merely reads is not.
#[test]
fn r2_h6_6_interpreter_write_position_only() {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}\n");
    fs::create_dir_all(fx.root.join(".git/hooks")).unwrap();
    run_corpus(
        &fx,
        &[
            (
                "python3 -c \"import json; d=json.load(open('app/config.json')); open('build/firebase_ids.txt','w').write(d['project_info']['project_id'])\"",
                ALLOW,
            ),
            (
                "python3 -c \"t=open('.git/config').read(); open('docs/remote.txt','w').write(t)\"",
                ALLOW,
            ),
            (
                "python3 -c \"import json; print(json.load(open('.claude/settings.json'))['x'])\"",
                ALLOW,
            ),
            (
                "node -e \"const fs=require('fs'); fs.writeFileSync('docs/env.md', fs.readFileSync('.env.example','utf8'))\"",
                ALLOW,
            ),
            (
                "python3 -c \"import shutil; shutil.copy('.claude/settings.json', 'backup/settings.json')\"",
                ALLOW,
            ),
            (
                "python3 -c \"import os; print(os.path.join('.git', 'config'))\"",
                ALLOW,
            ),
            (
                "node -e \"console.log('.env'.replace('.env', 'x'))\"",
                ALLOW,
            ),
            ("python3 -c \"l=['.env']; l.remove('.env')\"", ALLOW),
            // Gaps closed.
            (
                "ruby -e \"File.new('.git/hooks/pre-commit','w').puts('x')\"",
                INTERP,
            ),
            (
                "node -e \"const fs=require('fs'); fs.cp('x', '.git/hooks/pre-push', ()=>{})\"",
                INTERP,
            ),
            (
                "node -e \"const fs=require('fs'); const src='x'; fs.copyFileSync(src, '.git/hooks/pre-push')\"",
                INTERP,
            ),
            (
                "python3 -c \"from pathlib import Path; (Path('.claude')/'settings.json').write_text('{}')\"",
                INTERP,
            ),
            // Writes the round-1 fix caught stay caught.
            (
                "python3 -c \"p='.git/hooks/pre-commit'; open(p,'w').write('x')\"",
                INTERP,
            ),
            (
                "perl -e 'open my $f, \">\", \".git/hooks/pre-commit\"'",
                INTERP,
            ),
            (
                "python3 -c \"import os; os.rename('.claude/settings.json', 'x.json')\"",
                INTERP,
            ),
            (
                "python3 -c \"import os; os.open('.git/config', os.O_WRONLY)\"",
                INTERP,
            ),
            (
                "ruby -e \"require 'fileutils'; FileUtils.cp('x', '.claude/settings.json')\"",
                INTERP,
            ),
        ],
    );
    // A secret is still not read: that denial is the read check, not a write.
    let got = fx.bash(
        "python3 -c \"import json; d=json.load(open('app/google-services.json')); open('build/ids.txt','w').write(d['x'])\"",
    );
    assert!(
        reason(&got).contains("read secret file"),
        "expected the secret-read denial, got {got:?}"
    );
}

/// R2-H6-7: under `run-as <pkg>` a relative path is inside the app's own
/// data directory, not a top-level device directory.
#[test]
fn r2_h6_7_adb_run_as_relative_targets() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("adb shell run-as com.example.app rm -rf cache", ALLOW),
            ("adb shell 'run-as com.example.app rm -rf files/*'", ALLOW),
            (
                "adb -s emulator-5554 shell run-as com.example.app rm -r shared_prefs",
                ALLOW,
            ),
            // Still denied.
            ("adb shell rm -rf sdcard", ADB),
            ("adb shell run-as com.example.app rm -rf ../..", ADB),
            ("adb shell run-as com.example.app rm -rf '$HOME'", ADB),
            ("adb shell run-as com.example.app rm -rf /sdcard", ADB),
            (
                "adb shell 'run-as com.example.app true; rm -rf sdcard'",
                ADB,
            ),
        ],
    );
}
