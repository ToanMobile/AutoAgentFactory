//! Embedded domain skills pack (`templates/skills/*/SKILL.md`) mapped by profile.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use aaf_domain::ports::{PortError, PortResult};
use serde::{Deserialize, Serialize};

use crate::config::PROFILES;

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SkillTemplate {
    pub name: &'static str,
    pub description: &'static str,
    /// Empty means core skill included for every profile (`auto` included).
    pub profiles: &'static [&'static str],
    pub content: &'static str,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSyncOutcome {
    pub written: Vec<String>,
    /// Every existing skill not overwritten (union of `unchanged` and `skipped_modified`).
    pub skipped_existing: Vec<String>,
    /// Existing skills whose on-disk content already matches the bundled template.
    pub unchanged: Vec<String>,
    /// Existing skills with local customizations preserved because `force == false`.
    pub skipped_modified: Vec<String>,
}

/// Read-only drift snapshot for a skills directory against a target profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillsAudit {
    pub synced: Vec<String>,
    pub missing: Vec<String>,
    pub modified: Vec<String>,
}

const MOBILE_PROFILES: &[&str] = &["android", "ios", "flutter", "react-native", "expo"];
const MOBILE_AND_UNITY_PROFILES: &[&str] =
    &["android", "ios", "flutter", "react-native", "expo", "unity"];
const MOBILE_AND_WEB_PROFILES: &[&str] =
    &["android", "ios", "flutter", "react-native", "expo", "web"];
const UI_PROFILES: &[&str] = &[
    "android",
    "ios",
    "flutter",
    "react-native",
    "expo",
    "web",
    "unity",
];
const UNITY_PROFILES: &[&str] = &["unity"];
const WEB_PROFILES: &[&str] = &["web"];
const BACKEND_PROFILES: &[&str] = &["backend", "rust"];

const SKILLS: [SkillTemplate; 15] = [
    SkillTemplate {
        name: "aaf-workflow",
        description: "AutoAgentFactory task execution, gating, rework, and telemetry workflow.",
        profiles: &[],
        content: include_str!("../../../templates/skills/aaf-workflow/SKILL.md"),
    },
    SkillTemplate {
        name: "tdd-workflow",
        description: "Paired RED->GREEN regression oracle discipline, surviving-mutant guard, and anti-tautology rules.",
        profiles: &[],
        content: include_str!("../../../templates/skills/tdd-workflow/SKILL.md"),
    },
    SkillTemplate {
        name: "merge-conflict-resolver",
        description: "Safe Git merge/rebase conflict resolution preserving both intents with post-resolution verification.",
        profiles: &[],
        content: include_str!("../../../templates/skills/merge-conflict-resolver/SKILL.md"),
    },
    SkillTemplate {
        name: "qa-review",
        description: "Pre-PR diff audit, acceptance-criteria traceability, edge-case probing, and secret screening.",
        profiles: &[],
        content: include_str!("../../../templates/skills/qa-review/SKILL.md"),
    },
    SkillTemplate {
        name: "mobile-proof-qa",
        description: "Capture real Android/iOS UI strictly via mobile-mcp and verify with aaf proof.",
        profiles: MOBILE_AND_UNITY_PROFILES,
        content: include_str!("../../../templates/skills/mobile-proof-qa/SKILL.md"),
    },
    SkillTemplate {
        name: "store-compliance",
        description: "Android Play and Apple App Store release readiness checklist.",
        profiles: MOBILE_AND_UNITY_PROFILES,
        content: include_str!("../../../templates/skills/store-compliance/SKILL.md"),
    },
    SkillTemplate {
        name: "crash-triage",
        description: "Root-cause crash and ANR triage with paired RED->GREEN oracles.",
        profiles: &[],
        content: include_str!("../../../templates/skills/crash-triage/SKILL.md"),
    },
    SkillTemplate {
        name: "security-gate",
        description: "Trust-boundary, sandboxing, allowlist, path safety, and secret hygiene gate.",
        profiles: &[],
        content: include_str!("../../../templates/skills/security-gate/SKILL.md"),
    },
    SkillTemplate {
        name: "ui-ux-pro-max",
        description: "Cross-platform UI/UX standards: accessibility, hit targets, semantic tokens, motion, and anti-slop baseline.",
        profiles: UI_PROFILES,
        content: include_str!("../../../templates/skills/ui-ux-pro-max/SKILL.md"),
    },
    SkillTemplate {
        name: "taste-mobile-app",
        description: "Native mobile art direction and 4-state screen architecture for Compose, SwiftUI, Flutter, and React Native.",
        profiles: MOBILE_PROFILES,
        content: include_str!("../../../templates/skills/taste-mobile-app/SKILL.md"),
    },
    SkillTemplate {
        name: "impeccable",
        description: "Surface-mode selection, refinement vs. redesign discipline, and iterative UI craft/polish passes.",
        profiles: MOBILE_AND_WEB_PROFILES,
        content: include_str!("../../../templates/skills/impeccable/SKILL.md"),
    },
    SkillTemplate {
        name: "qa-visual",
        description: "Post-render empirical layout measurement, viewport/device capture, and visual evidence QA.",
        profiles: MOBILE_AND_WEB_PROFILES,
        content: include_str!("../../../templates/skills/qa-visual/SKILL.md"),
    },
    SkillTemplate {
        name: "unity-game-craft",
        description: "Unity 6 zero-GC hot paths, deterministic EditMode tests, safe area and .meta hygiene.",
        profiles: UNITY_PROFILES,
        content: include_str!("../../../templates/skills/unity-game-craft/SKILL.md"),
    },
    SkillTemplate {
        name: "web-frontend-qa",
        description: "Web TypeScript strictness, WCAG AA accessibility, Core Web Vitals and XSS defense.",
        profiles: WEB_PROFILES,
        content: include_str!("../../../templates/skills/web-frontend-qa/SKILL.md"),
    },
    SkillTemplate {
        name: "backend-service-gate",
        description: "Backend and Rust timeouts, idempotency, migrations, resource cleanup and PII-free logs.",
        profiles: BACKEND_PROFILES,
        content: include_str!("../../../templates/skills/backend-service-gate/SKILL.md"),
    },
];

pub fn all_skills() -> &'static [SkillTemplate] {
    &SKILLS
}

pub fn is_valid_profile(profile: &str) -> bool {
    profile == "auto" || PROFILES.contains(&profile)
}

/// Returns bundled skills applicable to `profile` (core `&[]` skills apply to all profiles).
pub fn skills_for_profile(profile: &str) -> Vec<&'static SkillTemplate> {
    SKILLS
        .iter()
        .filter(|s| s.profiles.is_empty() || s.profiles.contains(&profile))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirCheck {
    RealDir,
    Missing,
    Unsafe,
}

fn check_real_dir(dir: &Path) -> DirCheck {
    if dir.as_os_str().is_empty() {
        return DirCheck::Unsafe;
    }
    let mut cur = PathBuf::new();
    for comp in dir.components() {
        match comp {
            Component::Prefix(p) => cur.push(p.as_os_str()),
            Component::RootDir => cur.push(comp.as_os_str()),
            Component::CurDir => continue,
            Component::ParentDir => return DirCheck::Unsafe,
            Component::Normal(part) => {
                cur.push(part);
                match fs::symlink_metadata(&cur) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        if !is_macos_system_symlink(&cur) {
                            return DirCheck::Unsafe;
                        }
                    }
                    Ok(meta) if !meta.is_dir() => return DirCheck::Unsafe,
                    Ok(_) => {}
                    Err(err) if err.kind() == io::ErrorKind::NotFound => return DirCheck::Missing,
                    Err(_) => return DirCheck::Unsafe,
                }
            }
        }
    }
    DirCheck::RealDir
}

/// Read-only inspection of `dest_root` against the expected skills for `profile`.
/// Flags symlinked ancestor/skill directories and non-`NotFound` I/O errors as `modified`.
pub fn inspect_skills_dir(dest_root: &Path, profile: &str) -> SkillsAudit {
    let mut audit = SkillsAudit::default();
    let expected = skills_for_profile(profile);
    match check_real_dir(dest_root) {
        DirCheck::Missing => {
            audit.missing = expected.into_iter().map(|s| s.name.to_string()).collect();
            return audit;
        }
        DirCheck::Unsafe => {
            audit.modified = expected.into_iter().map(|s| s.name.to_string()).collect();
            return audit;
        }
        DirCheck::RealDir => {}
    }
    for skill in expected {
        let skill_dir = dest_root.join(skill.name);
        match fs::symlink_metadata(&skill_dir) {
            Ok(meta) if !meta.file_type().is_symlink() && meta.is_dir() => {}
            Ok(_) => {
                audit.modified.push(skill.name.to_string());
                continue;
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                audit.missing.push(skill.name.to_string());
                continue;
            }
            Err(_) => {
                audit.modified.push(skill.name.to_string());
                continue;
            }
        }
        let path = skill_dir.join("SKILL.md");
        match fs::symlink_metadata(&path) {
            Ok(meta) if !meta.file_type().is_symlink() && meta.is_file() => match fs::read(&path) {
                Ok(existing) if existing == skill.content.as_bytes() => {
                    audit.synced.push(skill.name.to_string());
                }
                _ => audit.modified.push(skill.name.to_string()),
            },
            Ok(_) => audit.modified.push(skill.name.to_string()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                audit.missing.push(skill.name.to_string());
            }
            Err(_) => audit.modified.push(skill.name.to_string()),
        }
    }
    audit
}

/// Writes all matching bundled `SKILL.md` files (or `--only` subset) into `dest_root/<name>/SKILL.md`.
pub fn sync_skills(
    dest_root: &Path,
    only: Option<&[String]>,
    force: bool,
) -> PortResult<SkillSyncOutcome> {
    sync_skills_for_profile(dest_root, None, only, force)
}

/// Writes bundled `SKILL.md` files filtered by `profile` (`None` = all 15 skills) into
/// `dest_root/<name>/SKILL.md`. Rejects path traversal, symlinks, and unknown profiles.
pub fn sync_skills_for_profile(
    dest_root: &Path,
    profile: Option<&str>,
    only: Option<&[String]>,
    force: bool,
) -> PortResult<SkillSyncOutcome> {
    validate_dest_root(dest_root)?;
    if let Some(p) = profile
        && !is_valid_profile(p)
    {
        return Err(PortError::Invalid(format!(
            "unknown profile {p:?}: expected auto or {}",
            PROFILES.join(", ")
        )));
    }
    let selected = select_skills(profile, only)?;

    ensure_real_dir(dest_root)?;

    let mut outcome = SkillSyncOutcome::default();
    for skill in selected {
        let skill_dir = dest_root.join(skill.name);
        ensure_real_dir(&skill_dir)?;

        let file_path = skill_dir.join("SKILL.md");
        match fs::symlink_metadata(&file_path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.is_file() {
                    return Err(PortError::Invalid(format!(
                        "{} is a symlink or not a regular file",
                        file_path.display()
                    )));
                }
                if !force {
                    let name = skill.name.to_string();
                    let existing = fs::read(&file_path)
                        .map_err(|e| PortError::Io(format!("read {}: {e}", file_path.display())))?;
                    if existing == skill.content.as_bytes() {
                        outcome.unchanged.push(name.clone());
                    } else {
                        outcome.skipped_modified.push(name.clone());
                    }
                    outcome.skipped_existing.push(name);
                    continue;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(PortError::Io(format!(
                    "stat {}: {err}",
                    file_path.display()
                )));
            }
        }
        atomic_write_file(&skill_dir, &file_path, skill.content)?;
        outcome.written.push(skill.name.to_string());
    }

    Ok(outcome)
}

fn select_skills(
    profile: Option<&str>,
    only: Option<&[String]>,
) -> PortResult<Vec<&'static SkillTemplate>> {
    if let Some(filter) = only
        && !filter.is_empty()
    {
        let mut selected = Vec::with_capacity(filter.len());
        for raw in filter {
            if raw.contains("..") || raw.contains('/') || raw.contains('\\') {
                return Err(PortError::Invalid(format!("invalid skill name {raw:?}")));
            }
            let Some(found) = SKILLS.iter().find(|s| s.name == raw) else {
                return Err(PortError::Invalid(format!("unknown skill {raw:?}")));
            };
            if !selected
                .iter()
                .any(|s: &&SkillTemplate| s.name == found.name)
            {
                selected.push(found);
            }
        }
        return Ok(selected);
    }

    Ok(match profile {
        Some(p) => skills_for_profile(p),
        None => SKILLS.iter().collect(),
    })
}

fn validate_dest_root(dest_root: &Path) -> PortResult<()> {
    if dest_root.as_os_str().is_empty() {
        return Err(PortError::Invalid("skills directory is empty".to_string()));
    }
    for comp in dest_root.components() {
        if matches!(comp, Component::ParentDir) {
            return Err(PortError::Invalid(format!(
                "skills path {} must not contain `..`",
                dest_root.display()
            )));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn is_macos_system_symlink(path: &Path) -> bool {
    matches!(path.to_str(), Some("/var" | "/tmp" | "/etc"))
}

#[cfg(not(target_os = "macos"))]
fn is_macos_system_symlink(_path: &Path) -> bool {
    false
}

/// Walks every path component and ensures each directory is a real non-symlink
/// directory, creating missing directories one component at a time.
fn ensure_real_dir(dir: &Path) -> PortResult<()> {
    let mut cur = PathBuf::new();
    for comp in dir.components() {
        match comp {
            Component::Prefix(p) => cur.push(p.as_os_str()),
            Component::RootDir => cur.push(comp.as_os_str()),
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(PortError::Invalid(format!(
                    "skills path {} must not contain `..`",
                    dir.display()
                )));
            }
            Component::Normal(part) => {
                cur.push(part);
                match fs::symlink_metadata(&cur) {
                    Ok(meta) => {
                        if meta.file_type().is_symlink() {
                            if is_macos_system_symlink(&cur) {
                                continue;
                            }
                            return Err(PortError::Invalid(format!(
                                "{} is a symlink",
                                cur.display()
                            )));
                        }
                        if !meta.is_dir() {
                            return Err(PortError::Invalid(format!(
                                "{} is not a directory",
                                cur.display()
                            )));
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {
                        match fs::create_dir(&cur) {
                            Ok(()) => {}
                            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                            Err(e) => {
                                return Err(PortError::Io(format!(
                                    "create {}: {e}",
                                    cur.display()
                                )));
                            }
                        }
                        let meta = fs::symlink_metadata(&cur)
                            .map_err(|e| PortError::Io(format!("stat {}: {e}", cur.display())))?;
                        if meta.file_type().is_symlink() || !meta.is_dir() {
                            return Err(PortError::Invalid(format!(
                                "{} is a symlink or not a directory",
                                cur.display()
                            )));
                        }
                    }
                    Err(err) => {
                        return Err(PortError::Io(format!("stat {}: {err}", cur.display())));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Writes `content` to a new temp file inside `dir` and renames it into place.
fn atomic_write_file(dir: &Path, dest: &Path, content: &str) -> PortResult<()> {
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".SKILL.md.aaf-tmp-{}-{seq}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| PortError::Io(format!("create temp {}: {e}", tmp.display())))?;
    if let Err(e) = file
        .write_all(content.as_bytes())
        .and_then(|()| file.flush())
    {
        drop(file);
        let _ = fs::remove_file(&tmp);
        return Err(PortError::Io(format!("write {}: {e}", tmp.display())));
    }
    drop(file);
    if let Err(e) = fs::rename(&tmp, dest) {
        let _ = fs::remove_file(&tmp);
        return Err(PortError::Io(format!(
            "rename {} -> {}: {e}",
            tmp.display(),
            dest.display()
        )));
    }
    Ok(())
}
