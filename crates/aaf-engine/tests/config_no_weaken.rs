#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The local file, the environment and the command line cannot weaken the
//! gate through `project.*` or `run.*`: the profile, the executor and its
//! command come from the committed file only, and auto-merge can be switched
//! on only there or by an explicit command-line flag.

use std::fs;
use std::path::Path;

use aaf_domain::config::StoreMode;
use aaf_domain::gate::StoreStatus;
use aaf_engine::config::{
    ConfigInputs, ConfigOverlay, EffectiveConfig, LOCAL_CONFIG_REL, REPO_CONFIG_REL, load,
};
use aaf_engine::store_compliance::scan;
use tempfile::TempDir;

const DEBUGGABLE: &str = r#"<manifest xmlns:android="http://schemas.android.com/apk/res/android">
    <application android:debuggable="true" />
</manifest>
"#;

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

struct Repo {
    dir: TempDir,
}

impl Repo {
    fn new(tier2: &str, local: Option<&str>) -> Repo {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join(REPO_CONFIG_REL), tier2);
        if let Some(body) = local {
            write(&dir.path().join(LOCAL_CONFIG_REL), body);
        }
        Repo { dir }
    }

    fn load(&self, env: &[(&str, &str)], cli: ConfigOverlay) -> EffectiveConfig {
        let lookup = |key: &str| {
            env.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
        };
        load(&ConfigInputs {
            global_config: None,
            repo_root: self.dir.path().to_path_buf(),
            env: &lookup,
            cli,
        })
        .unwrap()
    }
}

fn loaded(tier2: &str, local: Option<&str>, env: &[(&str, &str)]) -> EffectiveConfig {
    Repo::new(tier2, local).load(env, ConfigOverlay::default())
}

fn one_warning(cfg: &EffectiveConfig, key: &str) {
    assert_eq!(cfg.warnings.len(), 1, "{:?}", cfg.warnings);
    assert!(cfg.warnings[0].contains(key), "{:?}", cfg.warnings);
}

/// The audit's reproduction: an uncommitted local file names a non-mobile
/// profile on an Android project and the default `auto` scan stops applying.
#[test]
fn local_profile_cannot_switch_store_compliance_off() {
    let repo = Repo::new("", Some("[project]\nprofile = \"rust\"\n"));
    write(
        &repo.dir.path().join("app/src/main/AndroidManifest.xml"),
        DEBUGGABLE,
    );
    let cfg = repo.load(&[], ConfigOverlay::default());
    assert_eq!(cfg.repo.gate.store_compliance, StoreMode::Auto);
    assert_eq!(cfg.repo.project.profile, "auto");
    one_warning(&cfg, "project.profile");

    let report = scan(repo.dir.path(), &cfg.repo, true);
    assert_eq!(report.status, StoreStatus::Blocked, "{report:#?}");
    assert_eq!(report.critical, 1);
}

/// Even a committed non-mobile profile does not hide a mobile project from
/// the `auto` scan: applicability is decided from the repository content.
#[test]
fn committed_non_mobile_profile_does_not_suppress_a_detected_mobile_project() {
    for profile in ["rust", "web", "backend"] {
        let repo = Repo::new(&format!("[project]\nprofile = \"{profile}\"\n"), None);
        write(
            &repo.dir.path().join("app/src/main/AndroidManifest.xml"),
            DEBUGGABLE,
        );
        let cfg = repo.load(&[], ConfigOverlay::default());
        assert_eq!(cfg.repo.project.profile, profile);
        let report = scan(repo.dir.path(), &cfg.repo, true);
        assert_eq!(report.status, StoreStatus::Blocked, "{profile}");
    }
    // Allowed side: the same profiles on a repository with no mobile project.
    let repo = Repo::new("[project]\nprofile = \"rust\"\n", None);
    write(&repo.dir.path().join("src/lib.rs"), "pub fn f() {}\n");
    let cfg = repo.load(&[], ConfigOverlay::default());
    let report = scan(repo.dir.path(), &cfg.repo, true);
    assert_eq!(report.status, StoreStatus::NotApplicable);
}

#[test]
fn project_profile_comes_from_tier_two_only() {
    let tier2 = "[project]\nprofile = \"android\"\n";
    let cfg = loaded(tier2, Some("[project]\nprofile = \"web\"\n"), &[]);
    assert_eq!(cfg.repo.project.profile, "android");
    one_warning(&cfg, "project.profile");

    let mut cli = ConfigOverlay::default();
    cli.project.profile = Some("web".to_string());
    let cfg = Repo::new(tier2, None).load(&[], cli);
    assert_eq!(cfg.repo.project.profile, "android");
    one_warning(&cfg, "project.profile");

    // Repeating the tier 2 value is not an override attempt.
    let same = loaded(tier2, Some("[project]\nprofile = \"android\"\n"), &[]);
    assert!(same.warnings.is_empty(), "{:?}", same.warnings);

    // An unknown profile in the local file is dropped like any other.
    let cfg = loaded(tier2, Some("[project]\nprofile = \"nope\"\n"), &[]);
    assert_eq!(cfg.repo.project.profile, "android");
    one_warning(&cfg, "project.profile");

    // Still allowed: the display name follows the higher tier.
    let cfg = loaded(
        "[project]\nname = \"a\"\n",
        Some("[project]\nname = \"b\"\n"),
        &[],
    );
    assert_eq!(cfg.repo.project.name, "b");
    assert!(cfg.warnings.is_empty());
}

#[test]
fn auto_merge_cannot_be_turned_on_by_the_local_file_or_the_environment() {
    let cfg = loaded(
        "[run]\nauto_merge = false\n",
        Some("[run]\nauto_merge = true\n"),
        &[],
    );
    assert!(!cfg.repo.run.auto_merge);
    one_warning(&cfg, "run.auto_merge");

    let cfg = loaded(
        "[run]\nauto_merge = false\n",
        None,
        &[("AAF_AUTO_MERGE", "1")],
    );
    assert!(!cfg.repo.run.auto_merge);
    one_warning(&cfg, "run.auto_merge");
}

#[test]
fn auto_merge_can_be_turned_off_by_every_source_and_on_by_tier_two_or_a_cli_flag() {
    let on = "[run]\nauto_merge = true\n";
    assert!(loaded(on, None, &[]).repo.run.auto_merge);

    let cfg = loaded(on, Some("[run]\nauto_merge = false\n"), &[]);
    assert!(!cfg.repo.run.auto_merge);
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);

    let cfg = loaded(on, None, &[("AAF_AUTO_MERGE", "false")]);
    assert!(!cfg.repo.run.auto_merge);
    assert!(cfg.warnings.is_empty());

    // Repeating "on" where tier 2 already says so changes nothing.
    let cfg = loaded(on, Some("[run]\nauto_merge = true\n"), &[]);
    assert!(cfg.repo.run.auto_merge);
    assert!(cfg.warnings.is_empty());

    // What the local file switched off, the environment cannot switch back on.
    let cfg = loaded(
        on,
        Some("[run]\nauto_merge = false\n"),
        &[("AAF_AUTO_MERGE", "true")],
    );
    assert!(!cfg.repo.run.auto_merge);
    one_warning(&cfg, "run.auto_merge");

    // The explicit `--auto-merge` flag is the operator's own decision.
    let mut cli = ConfigOverlay::default();
    cli.run.auto_merge = Some(true);
    let cfg = Repo::new("", None).load(&[], cli.clone());
    assert!(cfg.repo.run.auto_merge);
    assert!(cfg.warnings.is_empty());
    let cfg = Repo::new("", Some("[run]\nauto_merge = false\n")).load(&[], cli);
    assert!(cfg.repo.run.auto_merge);

    let mut cli = ConfigOverlay::default();
    cli.run.auto_merge = Some(false);
    assert!(!Repo::new(on, None).load(&[], cli).repo.run.auto_merge);
}

#[test]
fn executor_and_its_command_come_from_tier_two_only() {
    let tier2 = "[run]\nexecutor = \"custom\"\nexecutor_cmd = [\"/usr/bin/safe\", \"{prompt}\"]\n";
    let local = "[run]\nexecutor = \"codex\"\nexecutor_cmd = [\"/bin/sh\", \"-c\", \"{prompt}\"]\n";
    let cfg = loaded(tier2, Some(local), &[]);
    assert_eq!(cfg.repo.run.executor, "custom");
    assert_eq!(cfg.repo.run.executor_cmd, vec!["/usr/bin/safe", "{prompt}"]);
    assert_eq!(cfg.warnings.len(), 2, "{:?}", cfg.warnings);
    assert!(cfg.warnings.iter().any(|w| w.contains("run.executor =")));
    assert!(cfg.warnings.iter().any(|w| w.contains("run.executor_cmd")));

    // With nothing committed the defaults stand; the local file adds no command.
    let cfg = loaded("", Some(local), &[]);
    assert_eq!(cfg.repo.run.executor, "claude");
    assert!(cfg.repo.run.executor_cmd.is_empty());
    assert_eq!(cfg.warnings.len(), 2);

    let mut cli = ConfigOverlay::default();
    cli.run.executor = Some("codex".to_string());
    cli.run.executor_cmd = Some(vec!["/bin/sh".to_string(), "{prompt}".to_string()]);
    let cfg = Repo::new(tier2, None).load(&[], cli);
    assert_eq!(cfg.repo.run.executor, "custom");
    assert_eq!(cfg.repo.run.executor_cmd, vec!["/usr/bin/safe", "{prompt}"]);
    assert_eq!(cfg.warnings.len(), 2);

    // Repeating the committed values is silent.
    let cfg = loaded(tier2, Some(tier2), &[]);
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}

/// The fields that only tune a run still follow the higher tier, and the
/// tightening direction of `gate.*` is untouched.
#[test]
fn harmless_run_fields_and_gate_tightening_still_follow_the_higher_tier() {
    let cfg = loaded(
        "[run]\nmax_parallel = 5\nrun_timeout_secs = 100\n\n[gate]\nstore_compliance = \"auto\"\n",
        Some(
            "[run]\nmax_parallel = 2\nrun_timeout_secs = 7\n\n[gate]\nstore_compliance = \"always\"\nmin_android_target_sdk = 40\n",
        ),
        &[("AAF_MAX_PARALLEL", "3")],
    );
    assert_eq!(cfg.repo.run.max_parallel, 3);
    assert_eq!(cfg.repo.run.run_timeout_secs, 7);
    assert_eq!(cfg.repo.gate.store_compliance, StoreMode::Always);
    assert_eq!(cfg.repo.gate.min_android_target_sdk, 40);
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}
