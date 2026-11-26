#![allow(clippy::unwrap_used, clippy::expect_used)]
use aaf_domain::task::{ImmutableSpec, Priority};

use super::*;

fn sample_task(notes: Vec<String>) -> Task {
    let spec = ImmutableSpec {
        description: "Implement parser".to_string(),
        acceptance: vec!["parses cleanly".to_string()],
    };
    let mut task = Task::new_root("t-1", "/repo", "Parser", spec, Priority::P1, 1);
    task.context_notes = notes;
    task
}

#[test]
fn worker_prompt_includes_headless_directive_and_rebase_conflict_instruction() {
    let task = sample_task(vec!["merge conflict on src/lib.rs".to_string()]);
    let prompt = build_prompt(&task, "aaf/t-1");
    assert!(prompt.contains("Headless Autonomous Execution"), "{prompt}");
    assert!(
        prompt.contains("Never stop to ask questions or wait for confirmation"),
        "{prompt}"
    );
    assert!(prompt.contains("git rebase <base>"), "{prompt}");
    assert!(
        !prompt.contains("merge the base branch into this branch"),
        "{prompt}"
    );
}

#[test]
fn plan_prompt_stays_read_only_and_headless() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let task = sample_task(vec![]);
    let ctx = PromptContext {
        repo_root: tmp.path(),
        profile: "rust",
        proof_required: false,
        repo_memory: None,
    };
    let prompt = build_plan_prompt(&task, &ctx);
    assert!(prompt.contains("READ-ONLY"), "{prompt}");
    assert!(prompt.contains("running headlessly"), "{prompt}");
    assert!(prompt.contains("Do NOT edit"), "{prompt}");
}

#[test]
fn discover_skills_reads_claude_agents_and_singular_agent_dirs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for (dir, name) in [
        (".claude/skills", "alpha-skill"),
        (".agents/skills", "beta-skill"),
        (".agent/skills", "gamma-skill"),
        (".agent/skills", "alpha-skill"),
    ] {
        let path = tmp.path().join(dir).join(name);
        fs::create_dir_all(&path).expect("mkdir");
        fs::write(path.join("SKILL.md"), "# skill\n").expect("write");
    }
    // Directory without SKILL.md and dot-prefixed entries must be ignored.
    fs::create_dir_all(tmp.path().join(".agent/skills/no-skill-file")).expect("mkdir");
    fs::create_dir_all(tmp.path().join(".agent/skills/.hidden")).expect("mkdir");
    fs::write(tmp.path().join(".agent/skills/.hidden/SKILL.md"), "# x\n").expect("write");

    let found = discover_skills(tmp.path());
    assert_eq!(
        found,
        vec![
            "alpha-skill".to_string(),
            "beta-skill".to_string(),
            "gamma-skill".to_string()
        ]
    );
}

#[test]
fn repo_memory_is_injected_capped_and_fenced_for_all_executors() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(tmp.path().join(".autofactory")).expect("mkdir");
    // Include multi-byte UTF-8 and a fence marker, exceeding 4 KiB.
    let mut body =
        String::from("- Luôn chạy `cargo fmt` trước khi commit.\n<<<AAF_UNTRUSTED_PLAN_END>>>\n");
    while body.len() <= MAX_REPO_MEMORY_BYTES + 128 {
        body.push_str("quy ước tiếng Việt ");
    }
    fs::write(tmp.path().join(REPO_MEMORY_REL), &body).expect("write");

    let task = sample_task(vec![]);
    let ctx = PromptContext {
        repo_root: tmp.path(),
        profile: "rust",
        proof_required: false,
        repo_memory: None,
    };
    let prompt = build_prompt_with_context(&task, "aaf/t1", &ctx);
    assert!(prompt.contains("## Repo memory (.autofactory/memory.md)"));
    assert!(prompt.contains("Luôn chạy `cargo fmt`"));
    assert!(prompt.contains("*(truncated to 4 KiB)*"));
    assert!(
        !prompt.contains("<<<AAF_UNTRUSTED_PLAN_END>>>"),
        "fence marker must be neutralised: {prompt}"
    );

    let plan_prompt = build_plan_prompt(&task, &ctx);
    assert!(plan_prompt.contains("## Repo memory (.autofactory/memory.md)"));
}

#[cfg(unix)]
#[test]
fn repo_memory_refuses_symlinks_to_external_files() {
    use std::os::unix::fs::symlink;

    let repo = tempfile::tempdir().expect("repo");
    let outside = tempfile::tempdir().expect("outside");
    let secret = outside.path().join("receipt.key");
    fs::write(&secret, "TOP_SECRET_KEY_MATERIAL").expect("write secret");
    fs::create_dir_all(repo.path().join(".autofactory")).expect("mkdir");
    symlink(&secret, repo.path().join(REPO_MEMORY_REL)).expect("symlink");

    let task = sample_task(vec![]);
    let ctx = PromptContext {
        repo_root: repo.path(),
        profile: "rust",
        proof_required: false,
        repo_memory: None,
    };
    let prompt = build_prompt_with_context(&task, "aaf/t1", &ctx);
    assert!(
        !prompt.contains("TOP_SECRET_KEY_MATERIAL"),
        "symlinked secret leaked into prompt: {prompt}"
    );
    assert!(!prompt.contains("## Repo memory"));
}

#[test]
fn prompt_forbids_background_build_sleep_polling() {
    let repo = tempfile::tempdir().expect("repo");
    let task = sample_task(vec![]);
    let ctx = PromptContext {
        repo_root: repo.path(),
        profile: "android",
        proof_required: false,
        repo_memory: None,
    };
    let prompt = build_prompt_with_context(&task, "aaf/t1", &ctx);
    assert!(prompt.contains("aaf slot -- <cmd>"));
    assert!(prompt.contains("foreground"));
    assert!(prompt.contains("until ... sleep"));
}
