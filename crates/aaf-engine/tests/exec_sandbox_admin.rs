#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The admin directory of a linked worktree (`<git dir>/worktrees/<name>`):
//! git writes inside it on every commit, but the entry itself must not be
//! renamed, removed or joined by a new sibling, or the guards on its hooks,
//! config and pointer files would no longer cover what git reads.

use aaf_domain::ports::SandboxPolicy;
use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile};

#[test]
fn admin_dir_entries_are_closed_with_every_metacharacter_of_the_path_escaped() {
    let dir = tempfile::tempdir().unwrap();
    let git_dir = dir
        .path()
        .canonicalize()
        .unwrap()
        .join("re.po+x (y) [z]{1}|^$*?/.git");
    let extra = SandboxPaths {
        git_dirs: vec![git_dir.to_string_lossy().into_owned()],
        ..SandboxPaths::default()
    };
    let profile = seatbelt_profile(&SandboxPolicy::default(), &extra).unwrap();

    // Spaces are literal in a POSIX regex; everything else is escaped.
    let entries = r#"/re\.po\+x \(y\) \[z\]\{1\}\|\^\$\*\?/\.git/worktrees(/[^/]+)?$"))"#;
    let deny = profile
        .lines()
        .find(|line| line.ends_with(entries))
        .unwrap_or_else(|| panic!("no admin entry rule in:\n{profile}"));
    assert!(deny.starts_with("(deny file-write* (regex #\"^/"), "{deny}");
    // The git dir itself must not be moved aside either.
    let own = format!("(deny file-write* (literal \"{}\"))", git_dir.display());
    assert!(profile.contains(&own), "{profile}");
    // SBPL applies the last matching rule: no allow may follow the guards.
    for rule in [deny, own.as_str()] {
        let at = profile.find(rule).unwrap();
        assert!(!profile[at..].contains("(allow "), "{profile}");
    }
}

#[cfg(target_os = "macos")]
mod real {
    use std::path::Path;

    use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, SandboxPolicy};
    use aaf_engine::executors::ExecEnv;
    use aaf_engine::executors::generic::GenericRunner;

    /// Identity and hook settings fixed so the user's global git config
    /// cannot change what the test does.
    const GIT_FLAGS: [&str; 8] = [
        "-c",
        "user.name=aaf-test",
        "-c",
        "user.email=aaf-test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "core.hooksPath=/dev/null",
    ];

    fn git(repo: &Path, args: &[&str]) -> String {
        let mut full = GIT_FLAGS.to_vec();
        full.extend_from_slice(args);
        aaf_fp::git(repo, &full, &[], aaf_fp::GIT_TIMEOUT).unwrap()
    }

    fn run_sandboxed(cwd: &Path, script: &str) -> AgentOutcome {
        let template = ["/bin/sh", "-c", script, "sh", "{prompt}"].map(String::from);
        let outcome = GenericRunner::new(template.to_vec(), ExecEnv::default())
            .unwrap()
            .run(&AgentRequest {
                run_id: format!("test-{}", uuid::Uuid::new_v4()),
                prompt: "unused".to_string(),
                cwd: cwd.to_string_lossy().into_owned(),
                account: None,
                timeout_secs: 60,
                sandbox: SandboxPolicy {
                    enabled: true,
                    ..SandboxPolicy::default()
                },
                json_schema: None,
            })
            .unwrap();
        assert!(!outcome.timed_out, "{}", outcome.log_tail);
        outcome
    }

    #[test]
    fn admin_dir_cannot_be_renamed_or_joined_while_commits_still_work() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        // Every character the rule's regex has to escape, in a real path.
        let (main, worktree) = (root.join("ma.in+(x) y"), root.join("wt"));
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q"]);
        std::fs::write(main.join("a.txt"), "a").unwrap();
        git(&main, &["add", "a.txt"]);
        git(&main, &["commit", "-q", "-m", "first"]);
        let wt = worktree.to_str().unwrap();
        git(&main, &["worktree", "add", "-q", "-b", "task", wt]);
        let git_dir = main.join(".git");
        let admin = git_dir.join("worktrees/wt");
        let commondir = std::fs::read(admin.join("commondir")).unwrap();

        let g = git_dir.display();
        let flags = GIT_FLAGS.join(" ");
        let script = format!(
            "echo new > b.txt && git add b.txt && git {flags} commit -q -m second && echo commit-ok; \
             mv '{g}/worktrees/wt' '{g}/worktrees/wt-old' || echo rename-denied; \
             mkdir '{g}/worktrees/evil' || echo sibling-denied; \
             mv '{g}/worktrees/wt' ./stolen-admin || echo move-out-denied; \
             rmdir '{g}/worktrees/wt' || echo remove-denied; \
             mv '{g}/worktrees' '{g}/worktrees-old' || echo parent-rename-denied; \
             mv '{g}/worktrees' ./stolen-worktrees || echo parent-move-out-denied; \
             mv '{g}' ./stolen-git || echo git-dir-move-denied; \
             echo more > c.txt && git add c.txt && git {flags} commit -q -m third && echo commit-again-ok"
        );
        let log = run_sandboxed(&worktree, &script).log_tail;
        for marker in [
            // The agent's own git work writes below the admin dir.
            "commit-ok",
            "rename-denied",
            "sibling-denied",
            "move-out-denied",
            "remove-denied",
            "parent-rename-denied",
            "parent-move-out-denied",
            "git-dir-move-denied",
            "commit-again-ok",
        ] {
            assert!(log.contains(marker), "missing {marker}: {log}");
        }
        assert!(admin.is_dir());
        for gone in ["worktrees/wt-old", "worktrees/evil", "worktrees-old"] {
            assert!(!git_dir.join(gone).exists(), "{gone}");
        }
        for stolen in ["stolen-admin", "stolen-worktrees", "stolen-git"] {
            assert!(!worktree.join(stolen).exists(), "{stolen}");
        }
        assert_eq!(std::fs::read(admin.join("commondir")).unwrap(), commondir);
        assert_eq!(git(&worktree, &["log", "-1", "--format=%s"]), "third");
        assert_eq!(git(&main, &["log", "-1", "--format=%s", "task"]), "third");
    }
}
