#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for the gaps a review found in the audit-round-4 fixes
//! (lane fix8-engine). One section per item; each has a refused and an
//! allowed case.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::json;

/// A Claude Code settings file whose only hook is `aaf-hook` with `entry`
/// merged into the hook entry.
fn settings_with(entry: serde_json::Value, env: Option<serde_json::Value>) -> String {
    let mut hook = json!({"type": "command", "command": "aaf-hook"});
    for (key, value) in entry.as_object().unwrap() {
        hook[key] = value.clone();
    }
    let mut root = json!({
        "hooks": {"PreToolUse": [{
            "matcher": "Bash|Write|Edit|MultiEdit|Read",
            "hooks": [hook],
        }]}
    });
    if let Some(env) = env {
        root["env"] = env;
    }
    root.to_string()
}

fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

// ---- Item 1: symlinks on the way to a closed cache path -------------------

/// The reviewer's two probes, plus a PATH entry that is a dangling link:
/// a dangling `init.d -> my-init` (the agent creates `my-init`), a chain
/// `bin -> hop -> real` with `hop` inside the writable cache (the agent
/// replaces `hop`), and `pbin -> pbin-real` on PATH.
#[cfg(target_os = "macos")]
#[test]
fn dangling_and_chained_links_to_closed_cache_paths_stay_closed() {
    use std::os::unix::fs::symlink;

    use aaf_domain::ports::{AgentRequest, AgentRunner, SandboxPolicy};
    use aaf_engine::executors::ExecEnv;
    use aaf_engine::executors::claude::ClaudeCodeRunner;

    if !Path::new("/usr/bin/sandbox-exec").exists() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    let gradle = home.join(".gradle");
    let pub_cache = home.join(".pub-cache");
    let wt = root.join("wt");
    for dir in [gradle.join("caches"), pub_cache.join("real"), wt.clone()] {
        fs::create_dir_all(dir).unwrap();
    }
    // Relative targets, as a person makes them with `ln -s my-init init.d`.
    symlink("my-init", gradle.join("init.d")).unwrap();
    symlink("real", pub_cache.join("hop")).unwrap();
    symlink("hop", pub_cache.join("bin")).unwrap();
    symlink("pbin-real", gradle.join("pbin")).unwrap();

    let q = |p: &Path| format!("'{}'", p.display());
    let probe = |marker: &str, commands: String| {
        format!("( {commands} ) 2>/dev/null && echo {marker}-ok || echo {marker}-denied\n")
    };
    let script = [
        "#!/bin/sh\n".to_string(),
        probe(
            "dangling",
            format!(
                "mkdir {} && echo evil > {}",
                q(&gradle.join("my-init")),
                q(&gradle.join("init.d/evil.gradle"))
            ),
        ),
        probe(
            "chain-swap",
            format!(
                "rm -f {hop} && mkdir {hop} && echo evil > {}",
                q(&pub_cache.join("bin/x")),
                hop = q(&pub_cache.join("hop"))
            ),
        ),
        probe(
            "chain-target",
            format!("echo evil > {}", q(&pub_cache.join("real/x"))),
        ),
        probe(
            "path-dangling",
            format!(
                "mkdir {} && echo evil > {}",
                q(&gradle.join("pbin-real")),
                q(&gradle.join("pbin/git"))
            ),
        ),
        probe(
            "cache",
            format!(
                "echo ok > {} && echo ok > {}",
                q(&gradle.join("caches/ok")),
                q(&pub_cache.join("cache-ok"))
            ),
        ),
    ]
    .concat();
    let claude = root.join("claude");
    fs::write(&claude, script).unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();

    let path_var = format!("/usr/bin:/bin:{}", gradle.join("pbin").display());
    let env = ExecEnv {
        home: Some(home.to_string_lossy().into_owned()),
        temp_dirs: Vec::new(),
        parent_env: vec![("PATH".to_string(), path_var)],
        ..ExecEnv::default()
    };
    let log = ClaudeCodeRunner::new(claude.to_string_lossy(), env)
        .run(&AgentRequest {
            run_id: format!("test-{}", uuid::Uuid::new_v4()),
            prompt: "unused".to_string(),
            cwd: wt.to_string_lossy().into_owned(),
            account: None,
            timeout_secs: 60,
            sandbox: SandboxPolicy {
                enabled: true,
                ..SandboxPolicy::default()
            },
        })
        .unwrap()
        .log_tail;

    for marker in [
        "dangling-denied",
        "chain-swap-denied",
        "chain-target-denied",
        "path-dangling-denied",
        "cache-ok",
    ] {
        assert!(log.contains(marker), "missing {marker}: {log}");
    }
    assert!(!gradle.join("my-init").exists(), "{log}");
    assert!(!gradle.join("pbin-real").exists(), "{log}");
    assert!(
        fs::symlink_metadata(pub_cache.join("hop"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(!pub_cache.join("real/x").exists());
}

// ---- Item 2: a hook timeout too short to finish ---------------------------

#[test]
fn hook_timeout_below_the_floor_is_rejected_and_the_floor_is_accepted() {
    use aaf_engine::hook_settings::covering_hook;

    for short in [1, 5, 10, 29] {
        let text = settings_with(json!({"timeout": short}), None);
        let err = covering_hook(&text).unwrap_err();
        assert!(err.contains("timeout"), "timeout={short}: {err}");
    }
    for enough in [30, 60, 600] {
        let text = settings_with(json!({"timeout": enough}), None);
        assert!(
            covering_hook(&text).is_ok(),
            "timeout={enough} was rejected"
        );
    }
    // No timeout: Claude Code's own default, which is what `aaf init` writes.
    assert!(covering_hook(&settings_with(json!({}), None)).is_ok());
}

// ---- Item 3: GIT_* variables in the settings' env -------------------------

/// The premise: with `GIT_CEILING_DIRECTORIES` set to the repository, the
/// git aaf-hook runs no longer finds the repository from a directory in it.
#[test]
fn a_git_ceiling_hides_the_repository_from_the_hooks_git() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().canonicalize().unwrap().join("repo");
    let sub = repo.join("src");
    fs::create_dir_all(&sub).unwrap();
    git_in(&repo, &["init", "-q", "-b", "main"]);

    let timeout = aaf_fp::GIT_TIMEOUT;
    let found = aaf_fp::git(&sub, &["rev-parse", "--show-toplevel"], &[], timeout).unwrap();
    assert_eq!(Path::new(&found), repo);
    let ceiling = [("GIT_CEILING_DIRECTORIES", repo.as_os_str())];
    assert!(aaf_fp::git(&sub, &["rev-parse", "--show-toplevel"], &ceiling, timeout).is_err());
}

#[test]
fn settings_env_refuses_every_git_variable_but_author_and_committer() {
    use aaf_engine::hook_settings::{check_local, covering_hook};

    for var in [
        "GIT_CEILING_DIRECTORIES",
        "GIT_OBJECT_DIRECTORY",
        "GIT_NAMESPACE",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        "GIT_REPLACE_REF_BASE",
        "git_ceiling_directories",
    ] {
        let env = json!({ var: "/tmp/attacker" });
        assert!(
            covering_hook(&settings_with(json!({}), Some(env.clone()))).is_err(),
            "settings.json env.{var} was accepted"
        );
        assert!(
            check_local(&json!({ "env": env }).to_string()).is_err(),
            "settings.local.json env.{var} was accepted"
        );
    }
    for var in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_EMAIL", "RUST_LOG"] {
        let env = json!({ var: "someone" });
        assert!(
            covering_hook(&settings_with(json!({}), Some(env.clone()))).is_ok(),
            "settings.json env.{var} was rejected"
        );
        assert!(
            check_local(&json!({ "env": env }).to_string()).is_ok(),
            "settings.local.json env.{var} was rejected"
        );
    }
}

// ---- Item 4: a committed settings.local.json git cannot read --------------

/// A repository whose base commits `.claude/settings.json` (registering a
/// hook found in `bin`) and, when `local` is given, `settings.local.json`.
fn repo_with_settings(
    local: Option<&str>,
) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let bin = tmp.path().join("bin");
    fs::create_dir_all(repo.join(".claude")).unwrap();
    fs::create_dir_all(&bin).unwrap();
    let hook = bin.join("aaf-hook");
    fs::write(&hook, "#!/bin/sh\necho '{}'\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    git_in(&repo, &["init", "-q", "-b", "main"]);
    fs::write(
        repo.join(".claude/settings.json"),
        settings_with(json!({}), None),
    )
    .unwrap();
    git_in(&repo, &["add", "-f", ".claude/settings.json"]);
    if let Some(local) = local {
        fs::write(repo.join(".claude/settings.local.json"), local).unwrap();
        git_in(&repo, &["add", "-f", ".claude/settings.local.json"]);
    }
    git_in(&repo, &["commit", "-q", "-m", "settings"]);
    (tmp, repo, bin)
}

#[test]
fn a_committed_local_settings_file_git_cannot_read_refuses_the_run() {
    use aaf_engine::hook_settings::check_base;

    let (_tmp, repo, bin) = repo_with_settings(Some("{\"permissions\": {}}\n"));
    // The tree names the file; its object is gone, so `git cat-file -e`
    // exits 1 as it would for a file that is not there.
    let oid = git_in(&repo, &["rev-parse", "main:.claude/settings.local.json"]);
    let object = repo.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
    fs::remove_file(object).unwrap();

    let err = check_base(&repo, "main", bin.as_os_str()).unwrap_err();
    assert!(err.contains("settings.local.json"), "{err}");
}

#[test]
fn a_base_without_local_settings_and_one_with_harmless_ones_pass() {
    use aaf_engine::hook_settings::check_base;

    let (_tmp, repo, bin) = repo_with_settings(None);
    check_base(&repo, "main", bin.as_os_str()).unwrap();
    let (_tmp, repo, bin) = repo_with_settings(Some("{\"permissions\": {}}\n"));
    check_base(&repo, "main", bin.as_os_str()).unwrap();
}

// ---- Item 5: the code-loading family in run.env_pass ----------------------

fn custom_run(env_pass: &[&str]) -> aaf_domain::config::RunConfig {
    aaf_domain::config::RunConfig {
        executor: "custom".to_string(),
        executor_cmd: vec!["/bin/echo".to_string(), "{prompt}".to_string()],
        env_pass: env_pass.iter().map(|name| name.to_string()).collect(),
        ..aaf_domain::config::RunConfig::default()
    }
}

/// Every variable that makes an interpreter load code it was not asked to.
const CODE_LOADING: [&str; 11] = [
    "JAVA_TOOL_OPTIONS",
    "_JAVA_OPTIONS",
    "JDK_JAVA_OPTIONS",
    "PYTHONPATH",
    "PYTHONHOME",
    "PERL5OPT",
    "PERL5LIB",
    "PERLLIB",
    "RUBYOPT",
    "RUBYLIB",
    "NODE_PATH",
];

#[test]
fn env_pass_refuses_the_whole_code_loading_family() {
    use aaf_domain::ports::PortError;
    use aaf_engine::executors::{ExecEnv, build_runner_with};

    for name in CODE_LOADING.iter().chain(&["java_tool_options"]) {
        let err = build_runner_with(&custom_run(&[name]), ExecEnv::default())
            .err()
            .unwrap_or_else(|| panic!("{name:?} was accepted"));
        let PortError::Invalid(message) = err else {
            panic!("{name:?}: {err:?}");
        };
        assert!(message.contains("loads code"), "{message}");
    }
    for ok in [
        "GRADLE_OPTS",
        "JAVA_HOME",
        "PYTHONUNBUFFERED",
        "PERL_BADLANG",
    ] {
        assert!(
            build_runner_with(&custom_run(&[ok]), ExecEnv::default()).is_ok(),
            "{ok:?} was refused"
        );
    }
}

// ---- Item 7: heavy_tool after commit e02ca68 -------------------------------

#[test]
fn arch_names_and_script_flags_do_not_hide_the_heavy_program() {
    use aaf_engine::throttle::heavy::{HeavyTool, heavy_tool, heavy_tool_argv};

    for argv in [
        // `-arm64e` is an architecture, not `-arm64` plus `-e <value>`.
        vec!["arch", "-arm64e", "./gradlew", "test"],
        vec!["arch", "-x86_64", "./gradlew", "test"],
        vec!["arch", "-e", "FOO=1", "./gradlew", "test"],
        vec!["arch", "-arch", "arm64e", "./gradlew", "test"],
        // macOS `script -F <pipe>`: `-F` is a flag, the pipe is the file.
        vec!["script", "-F", "/tmp/pipe", "./gradlew", "test"],
        vec!["script", "-qF", "/tmp/pipe", "./gradlew", "test"],
        vec!["script", "-t", "0", "/tmp/out", "./gradlew", "test"],
    ] {
        assert_eq!(
            heavy_tool_argv(&argv),
            Some(HeavyTool::Gradle),
            "heavy_tool_argv missed {argv:?}"
        );
        assert_eq!(
            heavy_tool(&argv.join(" ")),
            Some(HeavyTool::Gradle),
            "heavy_tool missed {argv:?}"
        );
    }
    for argv in [
        vec!["arch", "-arm64e", "echo", "gradle"],
        vec!["script", "-F", "/tmp/pipe", "echo", "gradle"],
        vec!["script", "-q", "gradlew.log"],
    ] {
        assert_eq!(heavy_tool_argv(&argv), None, "{argv:?}");
    }
}

/// A shell's script is the first word after its options, not the word right
/// after `-c` (found by lane fix8-hook in the hook's own parser).
#[test]
fn a_shell_script_after_more_options_is_still_classified() {
    use aaf_engine::throttle::heavy::{HeavyTool, heavy_tool, heavy_tool_argv};

    for argv in [
        vec!["sh", "-c", "--", "./gradlew build"],
        vec!["sh", "-c", "-", "./gradlew build"],
        vec!["bash", "-c", "-x", "./gradlew build"],
        vec!["bash", "-c", "-o", "pipefail", "./gradlew build"],
        vec!["bash", "-co", "pipefail", "./gradlew build"],
        vec!["bash", "-c", "--norc", "./gradlew build"],
        vec!["bash", "-c", "--rcfile", "/tmp/rc", "./gradlew build"],
        vec!["zsh", "-c", "+o", "nomatch", "./gradlew build"],
        vec!["zsh", "+o", "nomatch", "-c", "./gradlew build"],
        vec!["bash", "+O", "extglob", "-c", "./gradlew build"],
        vec!["bash", "--init-file", "/tmp/rc", "-c", "./gradlew build"],
    ] {
        assert_eq!(
            heavy_tool_argv(&argv),
            Some(HeavyTool::Gradle),
            "heavy_tool_argv missed {argv:?}"
        );
        let quoted: Vec<String> = argv.iter().map(|word| format!("'{word}'")).collect();
        assert_eq!(
            heavy_tool(&quoted.join(" ")),
            Some(HeavyTool::Gradle),
            "heavy_tool missed {argv:?}"
        );
    }
    // The words after the script are its `$0`, `$1`, …, and an option's
    // value is not a script.
    for argv in [
        vec!["sh", "-c", "echo hi", "gradle"],
        vec!["bash", "-c", "-o", "gradle", "echo hi"],
        vec!["bash", "--rcfile", "gradle", "-c", "echo hi"],
    ] {
        assert_eq!(heavy_tool_argv(&argv), None, "{argv:?}");
    }
}
