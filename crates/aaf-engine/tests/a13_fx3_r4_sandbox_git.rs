#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Audit a13 round 4. F7-R4-1: `commondir` of every git dir (the
//! superproject's and each submodule's) moves where git finds hooks and
//! config, so it is closed. F7-R4-2: neither `modules` nor an existing
//! submodule git dir can be renamed away, edited and renamed back.
//! F7-R4-3: a ref called `config` and a submodule called `libs/config` stay
//! usable while the submodule's own config is closed.
//! Round 5, F7-R5-1: a linked worktree of an existing submodule git dir
//! (`modules/sub/worktrees/<n>/`) is closed like one of the superproject.

use std::fs;
use std::path::{Path, PathBuf};

use aaf_domain::ports::SandboxPolicy;
use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile};

/// A git dir with an existing submodule git dir `modules/sub` (it holds
/// `HEAD`) and a directory `modules/new` that is not one.
fn git_dir_with_submodule(root: &Path) -> PathBuf {
    let git_dir = root.canonicalize().unwrap().join("repo/.git");
    fs::create_dir_all(git_dir.join("modules/sub/refs/heads")).unwrap();
    fs::create_dir_all(git_dir.join("modules/new")).unwrap();
    fs::write(git_dir.join("modules/sub/HEAD"), "ref: refs/heads/main\n").unwrap();
    git_dir
}

fn profile_for(git_dir: &Path) -> String {
    let extra = SandboxPaths {
        git_dirs: vec![git_dir.to_string_lossy().into_owned()],
        ..SandboxPaths::default()
    };
    seatbelt_profile(&SandboxPolicy::default(), &extra).unwrap()
}

/// `needle` is in `profile`, in a deny with no allow anywhere after it.
fn denied_last(profile: &str, needle: &str) {
    let at = profile
        .find(needle)
        .unwrap_or_else(|| panic!("missing {needle}:\n{profile}"));
    let deny = profile[..at].rfind("(deny file-write*").unwrap();
    assert!(!profile[deny..].contains("(allow "), "{profile}");
}

#[test]
fn profile_closes_commondir_of_the_git_dir_and_of_submodules() {
    let dir = tempfile::tempdir().unwrap();
    let git_dir = git_dir_with_submodule(dir.path());
    let profile = profile_for(&git_dir);
    let g = git_dir.to_str().unwrap();
    denied_last(&profile, &format!("(literal \"{g}/commondir\")"));
    denied_last(
        &profile,
        &format!("(literal \"{g}/modules/sub/commondir\")"),
    );
    denied_last(&profile, "|config\\.worktree|commondir)(/|$)\"");
}

#[test]
fn profile_closes_modules_and_existing_submodule_entries() {
    let dir = tempfile::tempdir().unwrap();
    let git_dir = git_dir_with_submodule(dir.path());
    let profile = profile_for(&git_dir);
    let g = git_dir.to_str().unwrap();
    denied_last(&profile, "(/worktrees/[^/]+)?/modules$\"");
    for entry in ["modules", "modules/sub"] {
        denied_last(&profile, &format!("(literal \"{g}/{entry}\")"));
    }
    for name in ["config", "config.worktree", "commondir"] {
        denied_last(&profile, &format!("(literal \"{g}/modules/sub/{name}\")"));
    }
    denied_last(&profile, &format!("(subpath \"{g}/modules/sub/hooks\")"));
    // The rule for submodules that do not exist yet leaves the existing one
    // to its exact rules.
    denied_last(
        &profile,
        &format!("(require-not (subpath \"{g}/modules/sub\"))"),
    );
    // `new` holds no HEAD: it is not a submodule git dir.
    assert!(
        !profile.contains(&format!("\"{g}/modules/new\"")),
        "{profile}"
    );
}

#[test]
fn profile_closes_linked_worktrees_of_an_existing_submodule_git_dir() {
    let dir = tempfile::tempdir().unwrap();
    let git_dir = git_dir_with_submodule(dir.path());
    let profile = profile_for(&git_dir);
    let g = git_dir.to_str().unwrap().replace('.', "\\.");
    for tail in [
        "/[^/]+/(hooks|config|config\\.worktree)(/|$)",
        "/[^/]+/(commondir|gitdir)(/|$)",
        "(/[^/]+)?$",
    ] {
        denied_last(&profile, &format!("\"^{g}/modules/sub/worktrees{tail}\""));
    }
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

    /// A repo whose git dir holds the submodule git dirs `modules/sub`,
    /// `modules/libs/config` and `modules/sub/modules/inner` (each with
    /// `HEAD`, `config`, `objects/` and `refs/heads/`), plus a linked
    /// worktree admin dir `modules/sub/worktrees/w`.
    fn repo_with_submodules(root: &Path) -> std::path::PathBuf {
        let repo = root.canonicalize().unwrap().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        let modules = repo.join(".git/modules");
        for sub in ["sub", "libs/config", "sub/modules/inner"] {
            let sub = modules.join(sub);
            fs::create_dir_all(sub.join("objects")).unwrap();
            fs::create_dir_all(sub.join("refs/heads")).unwrap();
            fs::write(sub.join("HEAD"), "ref: refs/heads/main\n").unwrap();
            fs::write(sub.join("config"), "[core]\n").unwrap();
        }
        let admin = modules.join("sub/worktrees/w");
        fs::create_dir_all(&admin).unwrap();
        fs::write(admin.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        fs::write(admin.join("gitdir"), "/nowhere/.git\n").unwrap();
        repo
    }

    fn run_in_sandbox(repo: &Path, script: &str) -> String {
        let template = ["/bin/sh", "-c", script, "sh", "{prompt}"].map(String::from);
        GenericRunner::new(template.to_vec(), ExecEnv::default())
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
            .unwrap()
            .log_tail
    }

    fn assert_markers(log: &str, markers: &[&str]) {
        for marker in markers {
            assert!(log.contains(marker), "missing {marker}: {log}");
        }
    }

    #[test]
    fn commondir_of_the_git_dir_and_of_a_submodule_cannot_be_written() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = repo_with_submodules(dir.path());
        let g = repo.join(".git");
        fs::create_dir_all(g.join("modules/fresh")).unwrap();
        let log = run_in_sandbox(
            &repo,
            "g=.git; \
             echo /tmp/x > $g/commondir || echo top-commondir-denied; \
             echo /tmp/x > $g/modules/sub/commondir || echo sub-commondir-denied; \
             echo /tmp/x > $g/modules/sub/modules/inner/commondir || echo inner-commondir-denied; \
             echo /tmp/x > $g/modules/fresh/commondir || echo fresh-commondir-denied",
        );
        assert_markers(
            &log,
            &[
                "top-commondir-denied",
                "sub-commondir-denied",
                "inner-commondir-denied",
                "fresh-commondir-denied",
            ],
        );
        for file in [
            "commondir",
            "modules/sub/commondir",
            "modules/sub/modules/inner/commondir",
            "modules/fresh/commondir",
        ] {
            assert!(!g.join(file).exists(), "{file} written: {log}");
        }
    }

    #[test]
    fn submodule_git_dirs_cannot_be_renamed_out_edited_and_renamed_back() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = repo_with_submodules(dir.path());
        let g = repo.join(".git");
        let log = run_in_sandbox(
            &repo,
            "g=.git; \
             mv $g/modules/sub $g/tmpsub || echo sub-mv-denied; \
             echo evil >> $g/tmpsub/config; mv $g/tmpsub $g/modules/sub; \
             mv $g/modules/sub/modules/inner $g/tmpinner || echo inner-mv-denied; \
             echo evil >> $g/tmpinner/config; mv $g/tmpinner $g/modules/sub/modules/inner; \
             mv $g/modules/libs $g/tmplibs || echo libs-mv-denied; \
             echo evil >> $g/tmplibs/config/config; mv $g/tmplibs $g/modules/libs; \
             mv $g/modules $g/m || echo modules-mv-denied; \
             echo evil >> $g/m/sub/config; mv $g/m $g/modules",
        );
        assert_markers(
            &log,
            &[
                "sub-mv-denied",
                "inner-mv-denied",
                "libs-mv-denied",
                "modules-mv-denied",
            ],
        );
        for sub in ["sub", "libs/config", "sub/modules/inner"] {
            let config = g.join("modules").join(sub).join("config");
            assert_eq!(fs::read_to_string(&config).unwrap(), "[core]\n", "{log}");
        }
    }

    #[test]
    fn refs_and_submodules_called_config_stay_writable_but_their_config_does_not() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = repo_with_submodules(dir.path());
        let g = repo.join(".git");
        let log = run_in_sandbox(
            &repo,
            "g=.git; \
             echo x > $g/modules/sub/refs/heads/config && echo ref-config-ok; \
             echo x > $g/modules/sub/refs/heads/hooks && echo ref-hooks-ok; \
             mkdir -p $g/modules/sub/logs/refs/heads && \
             echo x > $g/modules/sub/logs/refs/heads/config && echo log-config-ok; \
             echo evil >> $g/modules/sub/config || echo sub-config-denied; \
             echo x > $g/modules/libs/config/objects/tmp && echo libcfg-objects-ok; \
             echo x > $g/modules/libs/config/index && echo libcfg-index-ok; \
             echo evil >> $g/modules/libs/config/config || echo libcfg-config-denied; \
             mkdir $g/modules/libs/config/hooks || echo libcfg-hooks-denied",
        );
        assert_markers(
            &log,
            &[
                "ref-config-ok",
                "ref-hooks-ok",
                "log-config-ok",
                "sub-config-denied",
                "libcfg-objects-ok",
                "libcfg-index-ok",
                "libcfg-config-denied",
                "libcfg-hooks-denied",
            ],
        );
        let modules = g.join("modules");
        for sub in ["sub", "libs/config"] {
            let config = modules.join(sub).join("config");
            assert_eq!(fs::read_to_string(&config).unwrap(), "[core]\n", "{log}");
        }
        assert!(modules.join("sub/refs/heads/config").exists());
        assert!(modules.join("libs/config/objects/tmp").exists());
        assert!(!modules.join("libs/config/hooks").exists());
    }

    #[test]
    fn linked_worktrees_of_a_submodule_git_dir_cannot_be_redirected() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = repo_with_submodules(dir.path());
        let g = repo.join(".git");
        let log = run_in_sandbox(
            &repo,
            "g=.git; w=$g/modules/sub/worktrees/w; \
             echo /x > $w/commondir || echo subwt-commondir-denied; \
             echo /x > $w/gitdir || echo subwt-gitdir-denied; \
             echo x > $w/config.worktree || echo subwt-configwt-denied; \
             echo x > $w/config || echo subwt-config-denied; \
             mkdir $w/hooks || echo subwt-hooks-denied; \
             mkdir $g/modules/sub/worktrees/v || echo subwt-new-denied; \
             mv $w $g/modules/sub/worktrees/u || echo subwt-admin-mv-denied; \
             mv $g/modules/sub/worktrees $g/t || echo subwt-mv-denied",
        );
        assert_markers(
            &log,
            &[
                "subwt-commondir-denied",
                "subwt-gitdir-denied",
                "subwt-configwt-denied",
                "subwt-config-denied",
                "subwt-hooks-denied",
                "subwt-new-denied",
                "subwt-admin-mv-denied",
                "subwt-mv-denied",
            ],
        );
        let admin = g.join("modules/sub/worktrees/w");
        assert_eq!(
            fs::read_to_string(admin.join("commondir")).unwrap(),
            "../..\n"
        );
        for name in ["config.worktree", "config", "hooks"] {
            assert!(!admin.join(name).exists(), "{name} written: {log}");
        }
    }
}
