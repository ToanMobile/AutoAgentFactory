use super::*;

#[test]
fn global_skill_dirs_resolves_existing_and_explicit_hosts() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    // When no global host directories exist yet, fall back to ~/.claude/skills.
    assert_eq!(
        global_skill_dirs(home, GlobalSkillHost::ExistingOrDefault),
        vec![home.join(".claude/skills")]
    );

    // Creating ~/.gemini alone selects only ~/.gemini/config/skills.
    fs::create_dir_all(home.join(".gemini")).unwrap();
    assert_eq!(
        global_skill_dirs(home, GlobalSkillHost::ExistingOrDefault),
        vec![home.join(".gemini/config/skills")]
    );

    // Creating ~/.claude and ~/.codex selects all three in canonical order.
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(home.join(".codex")).unwrap();
    assert_eq!(
        global_skill_dirs(home, GlobalSkillHost::ExistingOrDefault),
        vec![
            home.join(".claude/skills"),
            home.join(".gemini/config/skills"),
            home.join(".codex/skills"),
        ]
    );

    // Explicit hosts resolve even when parent directories do not exist yet.
    let empty = tempfile::tempdir().unwrap();
    assert_eq!(
        global_skill_dirs(empty.path(), GlobalSkillHost::All),
        vec![
            empty.path().join(".claude/skills"),
            empty.path().join(".gemini/config/skills"),
            empty.path().join(".codex/skills"),
        ]
    );
    assert_eq!(
        global_skill_dirs(empty.path(), GlobalSkillHost::Gemini),
        vec![empty.path().join(".gemini/config/skills")]
    );
    assert_eq!(
        global_skill_dirs(empty.path(), GlobalSkillHost::Codex),
        vec![empty.path().join(".codex/skills")]
    );
}

#[cfg(unix)]
#[test]
fn global_skill_dirs_skips_symlinked_host_in_auto_discovery() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let target = tmp.path().join("outside");
    fs::create_dir_all(&target).unwrap();
    fs::create_dir_all(home.join(".claude")).unwrap();
    std::os::unix::fs::symlink(&target, home.join(".codex")).unwrap();

    assert_eq!(
        global_skill_dirs(home, GlobalSkillHost::ExistingOrDefault),
        vec![home.join(".claude/skills")]
    );
}
