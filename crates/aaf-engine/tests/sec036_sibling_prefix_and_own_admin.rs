#![allow(clippy::unwrap_used, clippy::expect_used)]
//! SEC-036, real Seatbelt: links in the git dir (and links on the way) open `wt_a` no sibling or
//! module state, only its own `index.lock`. Every probe works unsandboxed, so none fails by chance.

#[cfg(target_os = "macos")]
#[test]
fn links_in_the_git_dir_open_no_sibling_or_module_state() {
    use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile};
    let wt_a = "mkdir ../wt_a && echo \"gitdir: $(pwd -P)/worktrees/wt_a\" > ../wt_a/.git &&";
    let all = "(version 1)(allow default)";
    let cases = [
        (
            "mkdir -p alt/w/wt_a/hooks alt/w/wt_a2 e && ln -s alt/w x && ln -s x worktrees && ln -s ../../e alt/w/wt_b",
            "echo >$w/wt_a2/HEAD;echo >$w/wt_b/HEAD;echo >$w/wt_a/config;echo >$w/wt_a/hooks/x;mkdir -p $w/wt_a/modules/s/hooks;mv alt b;rm x",
        ),
        ("mkdir -p worktrees/wt_a && ln -s m modules", "mkdir -p m/s"),
        (
            "mkdir -p worktrees/wt_a alt/m e && ln -s alt/m y && ln -s y modules && ln -s ../../e alt/m/s",
            "echo >modules/s/HEAD;mkdir modules/n;mv alt b;rm y",
        ),
    ];
    for (setup, evil) in cases {
        let root = tempfile::tempdir().unwrap();
        let g = root.path().canonicalize().unwrap().join("git");
        std::fs::create_dir(&g).unwrap();
        let run = |profile: &str, script: &str| {
            let mut cmd = std::process::Command::new("/usr/bin/sandbox-exec");
            cmd.args(["-p", profile, "/bin/sh", "-c", script]);
            let out = cmd.current_dir(&g).output().unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        run(all, &format!("{wt_a} {setup}"));
        let probes = std::iter::once("echo >$w/wt_a/index.lock").chain(evil.split(';'));
        let script = probes.fold("w=worktrees\n".to_string(), |script, probe| {
            script + &format!("({probe}) 2>/dev/null && echo ok || echo no\n")
        });
        let extra = SandboxPaths {
            worktree: Some(g.with_file_name("wt_a").to_string_lossy().into_owned()),
            git_dirs: vec![g.to_string_lossy().into_owned()],
            ..SandboxPaths::default()
        };
        let profile = seatbelt_profile(&Default::default(), &extra).unwrap();
        let denied = format!("ok\n{}", "no\n".repeat(evil.split(';').count()));
        assert_eq!(run(&profile, &script), denied, "{setup}");
        assert_eq!(run(all, &script), denied.replace("no", "ok"), "{setup}");
    }
}
