#![allow(clippy::unwrap_used, clippy::expect_used)]
//! RED->GREEN regression corpus for the 5 P0 `aaf-hook` vulnerabilities:
//! 1. `rm -rf ~`, `$HOME`, `/Users/<user>`, and system directories
//! 2. `rm -rf <outside repo_root>` (allowing `/tmp/...` and `$TMPDIR/...` subpaths only)
//! 3. Dangerous delete/git/secret commands wrapped inside `python3 -c`, `node -e`, `perl -e`, `ruby -e`
//! 4. Secret file reads via `Read` tool (including symlinks) and `Bash` (`cat`, `head`, `< .env`, etc.)
//! 5. Zero false positives on normal commands, `.env.example`, `/tmp/sub`, comments, and heredocs

mod common;

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;

use common::{Fixture, run_corpus, run_file_corpus};

const DELETE: Option<&str> = Some("delete_guard");
const SECRET: Option<&str> = Some("secret_guard");
const INTERP: Option<&str> = Some("interp_guard");
const ALLOW: Option<&str> = None;

fn fixture_with_secrets() -> Fixture {
    let fx = Fixture::new();
    for f in [
        ".env",
        ".env.local",
        ".env.production",
        ".env.example",
        ".env.sample",
        ".env.template",
        "app/release.keystore",
        "app/upload.jks",
        "keystore.properties",
        "app/google-services.json",
        "certs/client.p12",
        "certs/private.pem",
        "keys/id_rsa",
        "README.md",
        "AGENTS.md",
        "src/main.go",
        "build/cache.bin",
    ] {
        fx.write(f, "dummy\n");
    }
    fx
}

#[test]
fn p0_1_blocks_recursive_rm_of_home_and_system_dirs() {
    let fx = fixture_with_secrets();
    let home = fx.home.display().to_string();
    let cases = [
        ("rm -rf ~".to_string(), DELETE),
        ("rm -rf ~/".to_string(), DELETE),
        ("rm -rf $HOME".to_string(), DELETE),
        ("rm -rf \"${HOME}\"".to_string(), DELETE),
        (format!("rm -rf {home}"), DELETE),
        ("rm -rf /".to_string(), DELETE),
        ("rm -rf /System".to_string(), DELETE),
        ("rm -rf /Library/Caches".to_string(), DELETE),
        ("rm -rf /usr/local".to_string(), DELETE),
        ("rm -rf /etc".to_string(), DELETE),
        ("rm -rf /Applications".to_string(), DELETE),
        ("rm -rf /Volumes".to_string(), DELETE),
        ("rm -rf /Users".to_string(), DELETE),
    ];
    let refs: Vec<(&str, Option<&str>)> = cases.iter().map(|(c, e)| (c.as_str(), *e)).collect();
    run_corpus(&fx, &refs);
}

#[test]
fn p0_2_blocks_recursive_rm_outside_repo_but_allows_tmp_subpaths() {
    let fx = fixture_with_secrets();
    let sibling = fx.root.parent().unwrap().join("sibling-repo");
    fs::create_dir_all(&sibling).unwrap();
    let cases = [
        (format!("rm -rf {}", sibling.display()), DELETE),
        ("rm -rf /Volumes/Data/Toan/Game".to_string(), DELETE),
        ("rm -rf ../sibling-repo".to_string(), DELETE),
        ("rm -rf $UNKNOWN_DIR".to_string(), DELETE),
        ("rm -rf \"$(pwd)/../other\"".to_string(), DELETE),
        ("rm -rf /tmp".to_string(), DELETE),
        ("rm -rf /private/tmp".to_string(), DELETE),
        // Allowed: inside repo or subpath under /tmp
        ("rm -rf build".to_string(), ALLOW),
        ("rm -rf /tmp/aaf-test-scratch-dir".to_string(), ALLOW),
        (
            "rm -rf /private/tmp/aaf-test-scratch-dir".to_string(),
            ALLOW,
        ),
    ];
    let refs: Vec<(&str, Option<&str>)> = cases.iter().map(|(c, e)| (c.as_str(), *e)).collect();
    run_corpus(&fx, &refs);
}

#[test]
fn p0_3_blocks_dangerous_ops_inside_interpreters_without_false_positives() {
    let fx = fixture_with_secrets();
    run_corpus(
        &fx,
        &[
            ("python3 -c \"import os; os.system('rm -rf /')\"", INTERP),
            (
                "python3 -c \"import os; os.system('rm -rf /Volumes/Data/Toan/Game')\"",
                INTERP,
            ),
            (
                "python3 -c \"import shutil; shutil.rmtree('/Volumes/Data/Toan/Game')\"",
                INTERP,
            ),
            (
                "python3 -c \"import shutil; shutil.rmtree(target)\"",
                INTERP,
            ),
            (
                "python3 -c \"import subprocess; subprocess.run(['git', 'reset', '--hard'])\"",
                INTERP,
            ),
            ("python3 -c \"print(open('.env').read())\"", INTERP),
            (
                "node -e \"require('fs').rmSync('/Volumes/Data/Toan/Game', {recursive: true})\"",
                INTERP,
            ),
            (
                "node -e \"require('child_process').execSync('git push --force origin main')\"",
                INTERP,
            ),
            (
                "node -e \"console.log(require('fs').readFileSync('.env', 'utf8'))\"",
                INTERP,
            ),
            ("perl -e \"system('rm -rf ~')\"", INTERP),
            (
                "ruby -e \"require 'fileutils'; FileUtils.rm_rf('/Users/alex')\"",
                INTERP,
            ),
            // Allowed normal scripts (0 false positives)
            (
                "python3 -c \"import json; print(json.dumps({'ok': True}))\"",
                ALLOW,
            ),
            ("python3 -c \"print(open('.env.example').read())\"", ALLOW),
            (
                "python3 -c \"import shutil; shutil.rmtree('/tmp/my-safe-dir')\"",
                ALLOW,
            ),
            ("node -e \"console.log(1 + 2)\"", ALLOW),
            ("perl -e \"print 'hello\\n'\"", ALLOW),
        ],
    );
}

#[test]
fn p0_4_blocks_secret_reads_via_read_tool_symlinks_and_bash() {
    let fx = fixture_with_secrets();
    #[cfg(unix)]
    {
        symlink(fx.root.join(".env"), fx.root.join("harmless.txt")).unwrap();
        symlink(fx.root.join(".env"), fx.root.join("fake.env.example")).unwrap();
    }

    run_file_corpus(
        &fx,
        &[
            ("Read", ".env", SECRET),
            ("Read", ".env.local", SECRET),
            ("Read", ".env.production", SECRET),
            ("Read", "app/release.keystore", SECRET),
            ("Read", "app/upload.jks", SECRET),
            ("Read", "keystore.properties", SECRET),
            ("Read", "app/google-services.json", SECRET),
            ("Read", "certs/client.p12", SECRET),
            ("Read", "certs/private.pem", SECRET),
            ("Read", "keys/id_rsa", SECRET),
            #[cfg(unix)]
            ("Read", "harmless.txt", SECRET),
            #[cfg(unix)]
            ("Read", "fake.env.example", SECRET),
            // Allowed public templates & normal files
            ("Read", ".env.example", ALLOW),
            ("Read", ".env.sample", ALLOW),
            ("Read", ".env.template", ALLOW),
            ("Read", "AGENTS.md", ALLOW),
            ("Read", "README.md", ALLOW),
            ("Edit", "src/main.go", ALLOW),
        ],
    );

    run_corpus(
        &fx,
        &[
            ("cat .env", SECRET),
            ("head -n 5 .env.local", SECRET),
            ("tail -f keystore.properties", SECRET),
            ("less app/google-services.json", SECRET),
            ("grep SECRET .env", SECRET),
            ("sed -n '1,10p' .env.production", SECRET),
            ("awk '{print}' certs/private.pem", SECRET),
            ("base64 app/release.keystore", SECRET),
            ("pbcopy < .env", SECRET),
            ("cat < .env", SECRET),
            #[cfg(unix)]
            ("cat harmless.txt", SECRET),
            // Allowed
            ("cat .env.example", ALLOW),
            ("cat .env.sample", ALLOW),
            ("cat .env.template", ALLOW),
            ("cat README.md", ALLOW),
            ("grep -n '.env' README.md", ALLOW),
            ("ls -la", ALLOW),
            ("git status", ALLOW),
            ("git diff --stat", ALLOW),
            ("aaf slot -- ./gradlew :app:testDebugUnitTest", ALLOW),
        ],
    );
}

#[test]
fn p0_5_comments_and_data_heredocs_have_zero_false_positives() {
    let fx = fixture_with_secrets();
    run_corpus(
        &fx,
        &[
            ("# rm -rf ~ && cat .env", ALLOW),
            (
                "cat <<'EOF'\nrm -rf /\npython3 -c \"import os; os.system('rm -rf /')\"\ncat .env\nEOF",
                ALLOW,
            ),
            ("echo 'rm -rf /Users/alex'", ALLOW),
        ],
    );
}
