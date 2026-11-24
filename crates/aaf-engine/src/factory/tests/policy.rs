//! The prompt and the sandbox policy of an agent run.

use std::fs;
use std::path::Path;

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::task::{Priority, Task};

use super::fakes::spec;
use super::world::World;
use crate::factory::prompt::{build_prompt, sandbox_policy, sandbox_policy_for};

fn profile(home: &Path, dir: &str) -> AccountProfile {
    let path = home.join(dir);
    fs::create_dir_all(&path).unwrap();
    AccountProfile {
        id: dir.trim_start_matches('.').to_string(),
        provider: ProviderKind::ClaudeCode,
        config_dir: path.to_string_lossy().into_owned(),
        name: dir.to_string(),
    }
}

#[test]
fn the_prompt_carries_the_spec_and_every_note_verbatim() {
    let mut task = Task::new_root("t1", "/repo", "Fix parser", spec(), Priority::P1, 1);
    let prompt = build_prompt(&task, "aaf/t1");
    assert!(prompt.contains("branch `aaf/t1`"));
    assert!(prompt.contains("# Task: Fix parser"));
    assert!(prompt.contains("Make the parser accept empty input."));
    assert!(prompt.contains("- empty input parses to an empty list\n"));
    assert!(!prompt.contains("Notes from earlier attempts"));

    let marker = "unblock[T3] digest=0123456789abcdef: previous attempt failed (gate_failed): x";
    task.context_notes = vec![marker.to_string(), "second note".to_string()];
    let prompt = build_prompt(&task, "aaf/t1");
    assert!(prompt.contains(&format!("- {marker}\n")), "{prompt}");
    assert!(prompt.contains("- second note\n"));
}

#[test]
fn a_sandboxed_run_cannot_read_the_data_dir_or_another_account() {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("data");
    let own = profile(home.path(), ".claude-work");
    let other = profile(home.path(), ".claude-private");

    let policy = sandbox_policy(
        true,
        &data,
        home.path(),
        Some(&own),
        &[own.clone(), other.clone()],
    );

    assert!(policy.enabled);
    assert_eq!(
        policy.read_denied_paths,
        vec![
            data.to_string_lossy().into_owned(),
            other.config_dir.clone()
        ]
    );
    assert!(
        policy.writable_paths.is_empty(),
        "the executor adds its own"
    );
}

#[test]
fn the_shared_claude_dir_and_the_own_account_stay_readable() {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("data");
    let shared = profile(home.path(), ".claude");
    let own = profile(home.path(), ".claude-work");
    // A profile directory that is only a link to the shared one.
    let alias_path = home.path().join(".claude-alias");
    std::os::unix::fs::symlink(&shared.config_dir, &alias_path).unwrap();
    let alias = AccountProfile {
        id: "alias".to_string(),
        config_dir: alias_path.to_string_lossy().into_owned(),
        ..shared.clone()
    };

    let policy = sandbox_policy(
        true,
        &data,
        home.path(),
        Some(&own),
        &[shared.clone(), own.clone(), alias],
    );

    // Profiles link `projects` and `settings` into ~/.claude: denying it
    // would break every other Claude account.
    assert_eq!(
        policy.read_denied_paths,
        vec![data.to_string_lossy().into_owned()]
    );

    // Run from another account, the work profile is closed again.
    let policy = sandbox_policy(
        true,
        &data,
        home.path(),
        Some(&shared),
        &[shared.clone(), own.clone()],
    );
    assert!(policy.read_denied_paths.contains(&own.config_dir));
}

#[test]
fn an_unsandboxed_run_gets_a_disabled_policy() {
    let home = tempfile::tempdir().unwrap();
    let other = profile(home.path(), ".claude-private");

    let policy = sandbox_policy(
        false,
        &home.path().join("data"),
        home.path(),
        None,
        &[other],
    );

    // Disabled is what makes the executor drop its permission-bypass flags.
    assert!(!policy.enabled);
    assert!(policy.read_denied_paths.is_empty());
}

#[test]
fn the_loop_passes_the_sandbox_setting_and_the_account_to_the_runner() {
    let world = World::new();
    world.add("t1");

    world.once();

    let request = world.runner.0.lock().unwrap().requests[0].clone();
    assert!(!request.sandbox.enabled, "the test world runs unsandboxed");
    assert_eq!(request.account.map(|a| a.id), Some("a".to_string()));
    assert_eq!(request.timeout_secs, world.repo.run.run_timeout_secs);
}

#[test]
fn a_sandboxed_loop_asks_for_the_sandbox_on_every_run() {
    let world = World::new();
    world.add("t1");
    let mut deps = world.deps();
    deps.settings.sandboxed = true;
    let hooks = crate::factory::Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };

    crate::factory::run(&deps, crate::factory::Mode::Once, &hooks).unwrap();

    let request = world.runner.0.lock().unwrap().requests[0].clone();
    assert!(request.sandbox.enabled);
    let data = world.tmp.path().to_string_lossy().into_owned();
    assert_eq!(request.sandbox.read_denied_paths, vec![data]);
}

#[test]
fn sandbox_policy_for_fails_closed_on_none_and_other_providers_and_exempts_antigravity_gemini() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let data = home.join("data");
    fs::create_dir_all(&data).unwrap();
    let claude = profile(home, ".claude");
    fs::write(home.join(".claude.json"), "{}").unwrap();
    let gemini_dir = home.join(".gemini");
    fs::create_dir_all(&gemini_dir).unwrap();
    let gemini_acct = AccountProfile {
        id: "gemini".to_string(),
        provider: ProviderKind::Antigravity,
        config_dir: gemini_dir.to_string_lossy().into_owned(),
        name: ".gemini".to_string(),
    };
    let other_agy = AccountProfile {
        id: "agy-work".to_string(),
        provider: ProviderKind::Antigravity,
        config_dir: {
            let p = home.join(".gemini-work");
            fs::create_dir_all(&p).unwrap();
            p.to_string_lossy().into_owned()
        },
        name: ".gemini-work".to_string(),
    };
    let all = vec![claude.clone(), gemini_acct.clone(), other_agy.clone()];
    let claude_json = home.join(".claude.json").to_string_lossy().into_owned();

    // None (custom/unknown) and Codex fail closed: deny .claude, .claude.json, and .gemini.
    for provider in [None, Some(ProviderKind::Codex)] {
        let p = sandbox_policy_for(true, &data, home, provider, None, &all);
        assert!(p.read_denied_paths.contains(&claude.config_dir));
        assert!(p.read_denied_paths.contains(&claude_json));
        assert!(p.read_denied_paths.contains(&gemini_acct.config_dir));
        assert!(p.read_denied_paths.contains(&other_agy.config_dir));
    }

    // Legacy wrapper `sandbox_policy(..., None, &all)` also fails closed.
    let wrapper = sandbox_policy(true, &data, home, None, &all);
    assert!(wrapper.read_denied_paths.contains(&claude.config_dir));
    assert!(wrapper.read_denied_paths.contains(&claude_json));
    assert!(wrapper.read_denied_paths.contains(&gemini_acct.config_dir));

    // Antigravity with own == None exempts ~/.gemini even when listed in `all`,
    // while denying ~/.claude, ~/.claude.json, and other Antigravity accounts.
    let agy_default = sandbox_policy_for(
        true,
        &data,
        home,
        Some(ProviderKind::Antigravity),
        None,
        &all,
    );
    assert!(
        !agy_default
            .read_denied_paths
            .contains(&gemini_acct.config_dir)
    );
    assert!(agy_default.read_denied_paths.contains(&claude.config_dir));
    assert!(agy_default.read_denied_paths.contains(&claude_json));
    assert!(
        agy_default
            .read_denied_paths
            .contains(&other_agy.config_dir)
    );

    // Antigravity with own == Some(other_agy) denies ~/.gemini to isolate accounts.
    let agy_named = sandbox_policy_for(
        true,
        &data,
        home,
        Some(ProviderKind::Antigravity),
        Some(&other_agy),
        &all,
    );
    assert!(
        agy_named
            .read_denied_paths
            .contains(&gemini_acct.config_dir)
    );
    assert!(!agy_named.read_denied_paths.contains(&other_agy.config_dir));
    assert!(agy_named.read_denied_paths.contains(&claude.config_dir));
    assert!(agy_named.read_denied_paths.contains(&claude_json));

    // Claude exempts ~/.claude and denies ~/.gemini.
    let claude_pol = sandbox_policy_for(
        true,
        &data,
        home,
        Some(ProviderKind::ClaudeCode),
        None,
        &all,
    );
    assert!(!claude_pol.read_denied_paths.contains(&claude.config_dir));
    assert!(!claude_pol.read_denied_paths.contains(&claude_json));
    assert!(
        claude_pol
            .read_denied_paths
            .contains(&gemini_acct.config_dir)
    );
}
