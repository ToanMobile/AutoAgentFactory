#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Plan v3 GAP-01 regression tests: safe host-side worktree hydration,
//! Tier-3 `[worktree]` rejection, secret separation, tracked `.gitignore`
//! enforcement, non-hardlink CoW cloning, and sandbox `write_denied` wiring.

#[path = "git_support.rs"]
mod git_support;

use std::fs;
use std::path::Path;

use aaf_domain::ports::SandboxPolicy;
use aaf_domain::worktree_config::WorktreeConfig;
use aaf_engine::config::{
    ConfigInputs, ConfigOverlay, LOCAL_CONFIG_REL, REPO_CONFIG_REL, load as load_config,
};
use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile};
use aaf_engine::factory::{BaseRef, GitOps, RealGit};
use aaf_engine::git::hydration::{
    assert_no_escaping_symlink, assert_tracked_gitignore, hydrate_for_agent,
    load_base_worktree_config, stage_for_gate, validate_relative_entry,
    worktree_write_denied_for_cwd,
};
use aaf_engine::git::{GitError, worktree};
use git_support::{Fixture, commit_file, git};

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn commit_nested(repo: &Path, rel: &str, content: &str, msg: &str) -> String {
    let full = repo.join(rel);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    commit_file(repo, rel, content, msg)
}

#[test]
fn copy_files_rejects_secrets_traversal_and_untracked_gitignore_entries() {
    let fx = Fixture::new();
    commit_file(
        &fx.repo,
        ".gitignore",
        ".env\ngoogle-services.json\nlocal.properties\n",
        "add gitignore",
    );
    write_file(&fx.repo.join(".env"), "API_KEY=secret\n");
    write_file(
        &fx.repo.join("google-services.json"),
        "{\"key\":\"secret\"}\n",
    );
    write_file(&fx.repo.join("local.properties"), "sdk.dir=/opt/sdk\n");

    // Ignored ONLY in `.git/info/exclude` (must be rejected!).
    write_file(
        &fx.repo.join(".git/info/exclude"),
        "exclude-only.properties\n",
    );
    write_file(&fx.repo.join("exclude-only.properties"), "local=1\n");

    // Not ignored anywhere.
    write_file(&fx.repo.join("unignored.txt"), "plain\n");

    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-reject", "main").unwrap();

    for bad_entry in [
        ".env",
        "google-services.json",
        "../../etc/passwd",
        "/etc/passwd",
        ".git/config",
        "exclude-only.properties",
        "unignored.txt",
        "README.md",
    ] {
        let cfg = WorktreeConfig {
            copy_files: vec![bad_entry.to_string()],
            ..WorktreeConfig::default()
        };
        let err = hydrate_for_agent(&fx.repo, &wt.path, &cfg).unwrap_err();
        assert!(
            matches!(err, GitError::Invalid(_)),
            "expected Invalid for copy_files={bad_entry:?}, got {err:?}"
        );
    }

    assert!(!wt.path.join(".env").exists());
    assert!(!wt.path.join("google-services.json").exists());
    assert!(!wt.path.join("exclude-only.properties").exists());

    // Direct validators also reject bad paths and `.git/info/exclude`.
    assert!(validate_relative_entry("").is_err());
    assert!(validate_relative_entry("/abs/path").is_err());
    assert!(validate_relative_entry("../escape").is_err());
    assert!(validate_relative_entry(".git/hooks").is_err());
    assert!(validate_relative_entry("a\0b").is_err());
    assert!(assert_tracked_gitignore(&fx.repo, Path::new("exclude-only.properties")).is_err());
}

#[test]
fn copy_files_copies_non_secret_gitignored_file_and_rejects_escaping_symlink() {
    let fx = Fixture::new();
    commit_file(
        &fx.repo,
        ".gitignore",
        "local.properties\nlinked.properties\n",
        "ignore properties",
    );
    write_file(&fx.repo.join("local.properties"), "sdk.dir=/opt/android\n");

    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("outside.txt");
    write_file(&outside_file, "outside-secret\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside_file, fx.repo.join("linked.properties")).unwrap();

    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-copy-ok", "main").unwrap();

    let ok_cfg = WorktreeConfig {
        copy_files: vec!["local.properties".to_string()],
        ..WorktreeConfig::default()
    };
    assert!(assert_no_escaping_symlink(&fx.repo, Path::new("local.properties")).is_ok());
    let hydrated = hydrate_for_agent(&fx.repo, &wt.path, &ok_cfg).unwrap();
    assert_eq!(
        hydrated.write_denied,
        vec![wt.path.join("local.properties")]
    );
    assert_eq!(
        fs::read_to_string(wt.path.join("local.properties")).unwrap(),
        "sdk.dir=/opt/android\n"
    );

    #[cfg(unix)]
    {
        assert!(assert_no_escaping_symlink(&fx.repo, Path::new("linked.properties")).is_err());
        let bad_cfg = WorktreeConfig {
            copy_files: vec!["linked.properties".to_string()],
            ..WorktreeConfig::default()
        };
        assert!(hydrate_for_agent(&fx.repo, &wt.path, &bad_cfg).is_err());
    }
}

#[test]
fn clone_dirs_clones_gitignored_dir_without_hardlinks_and_returns_write_denied() {
    let fx = Fixture::new();
    commit_file(
        &fx.repo,
        ".gitignore",
        "node_modules/\n",
        "ignore node_modules",
    );
    let src_file = fx.repo.join("node_modules/pkg/index.js");
    write_file(&src_file, "module.exports = 'original';\n");

    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-clone", "main").unwrap();
    let cfg = WorktreeConfig {
        clone_dirs: vec!["node_modules/".to_string()],
        ..WorktreeConfig::default()
    };

    let hydrated = hydrate_for_agent(&fx.repo, &wt.path, &cfg).unwrap();
    let dst_dir = wt.path.join("node_modules");
    assert_eq!(hydrated.write_denied, vec![dst_dir.clone()]);

    let dst_file = dst_dir.join("pkg/index.js");
    assert_eq!(
        fs::read_to_string(&dst_file).unwrap(),
        "module.exports = 'original';\n"
    );

    // Must NEVER be a hardlink: mutating cloned file must not mutate source.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let src_meta = fs::metadata(&src_file).unwrap();
        let dst_meta = fs::metadata(&dst_file).unwrap();
        assert_eq!(src_meta.nlink(), 1, "source must not be hardlinked");
        assert_eq!(dst_meta.nlink(), 1, "clone must not be hardlinked");
        assert_ne!(
            src_meta.ino(),
            dst_meta.ino(),
            "clone must have distinct inode"
        );
    }
    fs::write(&dst_file, "module.exports = 'mutated';\n").unwrap();
    assert_eq!(
        fs::read_to_string(&src_file).unwrap(),
        "module.exports = 'original';\n",
        "mutating cloned dir must not mutate source repo"
    );

    // Calling hydrate_for_agent again skips existing destination without overwriting.
    let hydrated_again = hydrate_for_agent(&fx.repo, &wt.path, &cfg).unwrap();
    assert_eq!(hydrated_again.write_denied, vec![dst_dir]);
    assert_eq!(
        fs::read_to_string(&dst_file).unwrap(),
        "module.exports = 'mutated';\n"
    );
}

#[test]
fn gate_copy_files_stages_secrets_and_cleans_up_on_drop() {
    let fx = Fixture::new();
    commit_file(
        &fx.repo,
        ".gitignore",
        ".env\napp/google-services.json\n",
        "ignore gate secrets",
    );
    write_file(&fx.repo.join(".env"), "SECRET_TOKEN=topsecret\n");
    write_file(
        &fx.repo.join("app/google-services.json"),
        "{\"project_id\":\"demo\"}\n",
    );

    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-gate-stage", "main").unwrap();
    let cfg = WorktreeConfig {
        gate_copy_files: vec![".env".to_string(), "app/google-services.json".to_string()],
        ..WorktreeConfig::default()
    };

    // hydrate_for_agent must NEVER copy gate_copy_files.
    let hydrated = hydrate_for_agent(&fx.repo, &wt.path, &cfg).unwrap();
    assert!(hydrated.write_denied.is_empty());
    assert!(!wt.path.join(".env").exists());
    assert!(!wt.path.join("app/google-services.json").exists());

    {
        let stage = stage_for_gate(&fx.repo, &wt.path, &cfg).unwrap();
        assert_eq!(stage.staged_paths().len(), 2);
        assert_eq!(
            fs::read_to_string(wt.path.join(".env")).unwrap(),
            "SECRET_TOKEN=topsecret\n"
        );
        assert_eq!(
            fs::read_to_string(wt.path.join("app/google-services.json")).unwrap(),
            "{\"project_id\":\"demo\"}\n"
        );
    }

    // After Drop, staged secret files are completely removed from worktree.
    assert!(!wt.path.join(".env").exists());
    assert!(!wt.path.join("app/google-services.json").exists());
}

#[test]
fn setup_cmd_runs_on_clean_worktree_and_refuses_dirty_worktree() {
    let fx = Fixture::new();
    commit_file(&fx.repo, ".gitignore", "setup.stamp\n", "ignore stamp");

    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-setup", "main").unwrap();
    let cfg = WorktreeConfig {
        setup_cmd: "printf 'ready' > setup.stamp".to_string(),
        ..WorktreeConfig::default()
    };

    hydrate_for_agent(&fx.repo, &wt.path, &cfg).unwrap();
    assert_eq!(
        fs::read_to_string(wt.path.join("setup.stamp")).unwrap(),
        "ready"
    );

    // Dirtying a tracked file causes setup_cmd to fail closed with GitError::Dirty.
    fs::write(wt.path.join("README.md"), "dirty\n").unwrap();
    let err = hydrate_for_agent(&fx.repo, &wt.path, &cfg).unwrap_err();
    assert!(
        matches!(err, GitError::Dirty(_)),
        "expected Dirty, got {err:?}"
    );
}

#[test]
fn tier3_local_worktree_section_is_rejected_and_base_sha_loader_is_immutable() {
    let fx = Fixture::new();
    commit_nested(
        &fx.repo,
        REPO_CONFIG_REL,
        "[worktree]\nclone_dirs = [\"node_modules\"]\ncopy_files = [\"local.properties\"]\n",
        "commit tier2 worktree config",
    );
    let base_sha = fx.head(&fx.repo, "HEAD");

    // Tier-3 config.local.toml attempting to override [worktree] must be ignored with a warning.
    write_file(
        &fx.repo.join(LOCAL_CONFIG_REL),
        "[worktree]\nclone_dirs = [\"evil\"]\ncopy_files = [\".env\"]\nsetup_cmd = \"echo pwn\"\n",
    );
    let no_env = |_: &str| None;
    let effective = load_config(&ConfigInputs {
        global_config: None,
        repo_root: fx.repo.clone(),
        env: &no_env,
        cli: ConfigOverlay::default(),
    })
    .unwrap();
    assert_eq!(effective.repo.worktree.clone_dirs, vec!["node_modules"]);
    assert_eq!(effective.repo.worktree.copy_files, vec!["local.properties"]);
    assert!(effective.repo.worktree.setup_cmd.is_empty());
    assert!(
        effective
            .warnings
            .iter()
            .any(|w| w.contains("[worktree]") && w.contains("config.local.toml")),
        "expected [worktree] Tier-3 rejection warning, got: {:?}",
        effective.warnings
    );

    // Uncommitted working-tree edits to .autofactory/config.toml must NOT affect base_sha load.
    write_file(
        &fx.repo.join(REPO_CONFIG_REL),
        "[worktree]\nclone_dirs = [\"tampered\"]\n",
    );
    let loaded = load_base_worktree_config(&fx.repo, &base_sha).unwrap();
    assert_eq!(loaded.clone_dirs, vec!["node_modules"]);
    assert_eq!(loaded.copy_files, vec!["local.properties"]);

    // Missing config at an earlier commit returns default.
    let first_sha = git(&fx.repo, &["rev-list", "--max-parents=0", "HEAD"]);
    assert_eq!(
        load_base_worktree_config(&fx.repo, &first_sha).unwrap(),
        WorktreeConfig::default()
    );

    // Invalid TOML committed at a ref fails closed.
    let bad_sha = commit_nested(
        &fx.repo,
        REPO_CONFIG_REL,
        "[worktree\nclone_dirs = [\"broken\"\n",
        "broken toml",
    );
    assert!(load_base_worktree_config(&fx.repo, &bad_sha).is_err());
}

#[test]
fn real_git_task_worktree_hydrates_and_wires_sandbox_write_denied() {
    let fx = Fixture::new();
    commit_file(
        &fx.repo,
        ".gitignore",
        "node_modules/\nlocal.properties\n",
        "add gitignore",
    );
    commit_nested(
        &fx.repo,
        REPO_CONFIG_REL,
        "[worktree]\nclone_dirs = [\"node_modules\"]\ncopy_files = [\"local.properties\"]\n",
        "add worktree config",
    );
    write_file(
        &fx.repo.join("node_modules/vitest/cli.js"),
        "console.log('ok');\n",
    );
    write_file(&fx.repo.join("local.properties"), "sdk.dir=/opt/sdk\n");

    let real_git = RealGit::new(
        fx.repo.clone(),
        fx.worktrees.clone(),
        fx.locks.clone(),
        BaseRef {
            remote: None,
            branch: "main".to_string(),
        },
    );
    let wt = real_git.task_worktree("t-hyd-1").unwrap();
    assert!(wt.path.join("node_modules/vitest/cli.js").is_file());
    assert!(wt.path.join("local.properties").is_file());

    let denied = worktree_write_denied_for_cwd(&wt.path).unwrap();
    let canon_wt = fs::canonicalize(&wt.path).unwrap();
    assert_eq!(
        denied,
        vec![
            canon_wt.join("node_modules"),
            canon_wt.join("local.properties"),
        ]
    );

    let extra = SandboxPaths {
        worktree: Some(canon_wt.to_string_lossy().into_owned()),
        write_denied: denied
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        ..SandboxPaths::default()
    };
    let policy = SandboxPolicy {
        enabled: true,
        read_only: false,
        writable_paths: Vec::new(),
        read_denied_paths: Vec::new(),
    };
    let profile = seatbelt_profile(&policy, &extra).unwrap();
    let nm_rule = format!(
        "(deny file-write* (subpath \"{}\"))",
        canon_wt.join("node_modules").display()
    );
    let lp_rule = format!(
        "(deny file-write* (subpath \"{}\"))",
        canon_wt.join("local.properties").display()
    );
    assert!(profile.contains(&nm_rule), "{profile}");
    assert!(profile.contains(&lp_rule), "{profile}");

    #[cfg(target_os = "macos")]
    {
        use aaf_domain::ports::{AgentRequest, AgentRunner};
        use aaf_engine::executors::ExecEnv;
        use aaf_engine::executors::generic::GenericRunner;

        let exec_env = ExecEnv {
            temp_dirs: vec!["/tmp".to_string()],
            worktrees_root: Some(fx.worktrees.to_string_lossy().into_owned()),
            ..ExecEnv::default()
        };
        let runner = GenericRunner::new(
            vec!["/bin/sh".into(), "-c".into(), "{prompt}".into()],
            exec_env,
        )
        .unwrap();
        let make_req = |prompt: &str| AgentRequest {
            run_id: format!("hyd-{}", uuid::Uuid::new_v4()),
            prompt: prompt.to_string(),
            cwd: wt.path.to_string_lossy().into_owned(),
            account: None,
            timeout_secs: 30,
            sandbox: SandboxPolicy {
                enabled: true,
                read_only: false,
                writable_paths: Vec::new(),
                read_denied_paths: Vec::new(),
            },
            json_schema: None,
            max_turns: None,
        };

        // Writing inside hydrated clone_dirs must be blocked by Seatbelt.
        let poison_nm = runner
            .run(&make_req("printf 'pwn' > node_modules/vitest/cli.js"))
            .unwrap();
        assert!(
            !poison_nm.succeeded(),
            "expected write_denied on node_modules: {poison_nm:?}"
        );
        assert_eq!(
            fs::read_to_string(wt.path.join("node_modules/vitest/cli.js")).unwrap(),
            "console.log('ok');\n"
        );

        // Overwriting hydrated copy_files must be blocked by Seatbelt.
        let poison_lp = runner
            .run(&make_req("printf 'pwn' > local.properties"))
            .unwrap();
        assert!(
            !poison_lp.succeeded(),
            "expected write_denied on local.properties: {poison_lp:?}"
        );
        assert_eq!(
            fs::read_to_string(wt.path.join("local.properties")).unwrap(),
            "sdk.dir=/opt/sdk\n"
        );

        // Writing normal source files inside the worktree must still succeed.
        let normal_write = runner.run(&make_req("printf 'ok' > normal.txt")).unwrap();
        assert!(
            normal_write.succeeded(),
            "expected normal write to succeed: {normal_write:?}"
        );
        assert_eq!(
            fs::read_to_string(wt.path.join("normal.txt")).unwrap(),
            "ok"
        );
    }
}
