#![allow(clippy::unwrap_used, clippy::expect_used)]
//! R10-L7-1: a Gemini / Antigravity or OpenCode account dir is writable for
//! session state, but what a later unsandboxed session of that account runs
//! or trusts (settings with hooks / MCP servers, trusted hooks, extensions,
//! plugins, bundled binaries) stays closed under Seatbelt.

#[cfg(target_os = "macos")]
mod seatbelt {
    use std::fs;
    use std::path::Path;

    use aaf_domain::account::{AccountProfile, ProviderKind};
    use aaf_domain::ports::{AgentRequest, AgentRunner, SandboxPolicy};
    use aaf_engine::executors::ExecEnv;
    use aaf_engine::executors::generic::GenericRunner;
    use tempfile::TempDir;

    fn probe(tag: &str, p: &Path) -> String {
        format!(
            "( echo x > '{}' ) 2>/dev/null && echo {tag}-ok || echo {tag}-denied\n",
            p.display()
        )
    }

    fn run_probes(provider: ProviderKind, acct: &Path, wt: &Path, script: &str) -> String {
        let runner = GenericRunner::new(
            ["/bin/sh", "-c", script, "gemini", "{prompt}"]
                .map(String::from)
                .to_vec(),
            ExecEnv {
                temp_dirs: Vec::new(),
                parent_env: vec![("PATH".to_string(), "/usr/bin:/bin".to_string())],
                ..ExecEnv::default()
            },
        )
        .unwrap();
        runner
            .run(&AgentRequest {
                run_id: format!("test-r10-acct-{}-{}", provider.as_str(), std::process::id()),
                prompt: "go".to_string(),
                cwd: wt.to_string_lossy().into_owned(),
                account: Some(AccountProfile {
                    id: "acct-test".to_string(),
                    provider,
                    config_dir: acct.to_string_lossy().into_owned(),
                    name: "test".to_string(),
                }),
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

    #[test]
    fn gemini_account_config_closed_state_open_in_seatbelt() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let wt = root.join("wt");
        let gemini = root.join("gemini-acct");
        for d in [
            wt.clone(),
            gemini.join("tmp"),
            gemini.join("extensions"),
            gemini.join("antigravity/bin"),
        ] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(gemini.join("settings.json"), "{}").unwrap();
        let script = [
            probe("g-tmp", &gemini.join("tmp/x")),
            probe("g-creds", &gemini.join("oauth_creds.json")),
            probe("g-settings", &gemini.join("settings.json")),
            probe("g-trusted", &gemini.join("trusted_hooks.json")),
            probe("g-ext", &gemini.join("extensions/evil.json")),
            probe("g-bin", &gemini.join("antigravity/bin/agentapi")),
            probe("g-cli", &gemini.join("antigravity-cli/settings.json")),
        ]
        .concat();
        let log = run_probes(ProviderKind::Antigravity, &gemini, &wt, &script);
        for marker in [
            "g-tmp-ok",
            "g-creds-denied",
            "g-settings-denied",
            "g-trusted-denied",
            "g-ext-denied",
            "g-bin-denied",
            "g-cli-denied",
        ] {
            assert!(log.contains(marker), "missing {marker}: {log}");
        }
        assert_eq!(
            fs::read_to_string(gemini.join("settings.json")).unwrap(),
            "{}"
        );
    }

    #[test]
    fn opencode_account_config_closed_in_seatbelt() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let wt = root.join("wt");
        let oc = root.join("opencode-acct");
        for d in [wt.clone(), oc.join("plugin")] {
            fs::create_dir_all(d).unwrap();
        }
        let script = [
            probe("o-state", &oc.join("scratch.txt")),
            probe("o-cfg", &oc.join("opencode.json")),
            probe("o-cfgc", &oc.join("opencode.jsonc")),
            probe("o-plugin", &oc.join("plugin/evil.ts")),
            probe("o-plugins", &oc.join("plugins/evil.ts")),
        ]
        .concat();
        let log = run_probes(ProviderKind::OpenCode, &oc, &wt, &script);
        for marker in [
            "o-state-ok",
            "o-cfg-denied",
            "o-cfgc-denied",
            "o-plugin-denied",
            "o-plugins-denied",
        ] {
            assert!(log.contains(marker), "missing {marker}: {log}");
        }
    }
}
