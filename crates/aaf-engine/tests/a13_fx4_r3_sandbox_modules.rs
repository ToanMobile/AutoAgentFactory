#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Audit a13 round 3 (F7-R3-1): the hooks and config of a submodule's git
//! dir (`<git_dir>/modules/<name>/`, and below a linked worktree's admin
//! dir) are run or obeyed by the owner's later git commands, so the sandbox
//! keeps them closed like the superproject's own.

use aaf_domain::ports::SandboxPolicy;
use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile};

#[test]
fn profile_closes_submodule_hooks_and_config_with_an_escaped_path() {
    let dir = tempfile::tempdir().unwrap();
    let git_dir = dir.path().canonicalize().unwrap().join("re.po+x/.git");
    let extra = SandboxPaths {
        git_dirs: vec![git_dir.to_string_lossy().into_owned()],
        ..SandboxPaths::default()
    };
    let profile = seatbelt_profile(&SandboxPolicy::default(), &extra).unwrap();
    let rule = r#"re\.po\+x/\.git(/worktrees/[^/]+)?/modules/.+/(hooks|config|config\.worktree|commondir)(/|$)")"#;
    let at = profile.find(rule).unwrap_or_else(|| panic!("{profile}"));
    let deny = profile[..at].rfind("(deny file-write*").unwrap();
    assert!(!profile[deny..].contains("(allow "), "{profile}");
}

#[cfg(target_os = "macos")]
mod real {
    use std::fs;
    use std::path::Path;

    use aaf_domain::ports::{AgentRequest, AgentRunner, SandboxPolicy};
    use aaf_engine::executors::ExecEnv;
    use aaf_engine::executors::generic::GenericRunner;

    fn git(repo: &Path, args: &[&str]) {
        let mut full = vec!["-c", "user.name=t", "-c", "user.email=t@example.invalid"];
        full.extend_from_slice(args);
        aaf_fp::git(repo, &full, &[], aaf_fp::GIT_TIMEOUT).unwrap();
    }

    #[test]
    fn submodule_hooks_and_config_are_closed_but_objects_and_index_are_not() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().canonicalize().unwrap().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        let modules = repo.join(".git/modules");
        for sub in ["sub/hooks", "sub/objects", "libs/foo/hooks"] {
            fs::create_dir_all(modules.join(sub)).unwrap();
        }
        for file in ["sub/config", "libs/foo/config"] {
            fs::write(modules.join(file), "[core]\n").unwrap();
        }
        let script = "m=.git/modules; \
             echo evil >> $m/sub/config || echo sub-config-denied; \
             echo evil > $m/sub/config.worktree || echo sub-config-worktree-denied; \
             echo evil > $m/sub/hooks/post-checkout || echo sub-hook-denied; \
             echo evil >> $m/libs/foo/config || echo nested-config-denied; \
             echo evil > $m/libs/foo/hooks/pre-commit || echo nested-hook-denied; \
             echo x > $m/sub/objects/tmp && echo objects-ok; \
             echo x > $m/sub/index && echo index-ok";
        let template = ["/bin/sh", "-c", script, "sh", "{prompt}"].map(String::from);
        let outcome = GenericRunner::new(template.to_vec(), ExecEnv::default())
            .unwrap()
            .run(&AgentRequest {
                run_id: format!("test-{}", uuid::Uuid::new_v4()),
                prompt: "unused".to_string(),
                cwd: repo.to_string_lossy().into_owned(),
                account: None,
                timeout_secs: 60,
                sandbox: SandboxPolicy {
                    enabled: true,
                    ..SandboxPolicy::default()
                },
                json_schema: None,
                max_turns: None,
            })
            .unwrap();
        let log = outcome.log_tail;
        for marker in [
            "sub-config-denied",
            "sub-config-worktree-denied",
            "sub-hook-denied",
            "nested-config-denied",
            "nested-hook-denied",
            "objects-ok",
            "index-ok",
        ] {
            assert!(log.contains(marker), "missing {marker}: {log}");
        }
        for file in ["sub/config", "libs/foo/config"] {
            assert_eq!(fs::read_to_string(modules.join(file)).unwrap(), "[core]\n");
        }
        assert!(!modules.join("sub/hooks/post-checkout").exists());
        assert!(!modules.join("sub/config.worktree").exists());
        assert!(modules.join("sub/objects/tmp").exists() && modules.join("sub/index").exists());
    }
}
