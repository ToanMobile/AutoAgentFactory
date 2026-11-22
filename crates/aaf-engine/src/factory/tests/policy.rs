//! The prompt and the sandbox policy of an agent run.

use std::fs;
use std::path::Path;

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::task::{Priority, Task};

use super::fakes::spec;
use super::world::World;
use crate::factory::prompt::{build_prompt, sandbox_policy};

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
