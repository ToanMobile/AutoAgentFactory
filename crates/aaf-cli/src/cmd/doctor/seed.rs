//! `aaf doctor` checks for CoW volume alignment (P0.3) and cache seed dry-run (S7).
//!
//! Strictly read-only: never creates `worktrees_dir` or any probe file.

use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use aaf_domain::worktree_config::WorktreeConfig;
use aaf_engine::git::hydration::device::{
    MountEntry, device_id, find_mount_for_path, nearest_existing_canonical, read_macos_mounts,
};
use aaf_engine::git::hydration::seed::{SEED_CACHE_ENV, dry_run_seed, seed_cache_enabled_from};

use super::{Check, advisory, not_applicable};
use crate::ctx::Ctx;

/// Resolves the nearest existing ancestor via `canonicalize` and re-appends
/// any non-existent trailing components lexically, without touching disk.
fn logical_canonical(path: &Path, cwd: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut missing_tail = Vec::new();
    let mut cur = abs.as_path();
    loop {
        if fs::metadata(cur).is_ok() {
            let mut base = fs::canonicalize(cur).unwrap_or_else(|_| cur.to_path_buf());
            for comp in missing_tail.into_iter().rev() {
                match comp {
                    Component::CurDir => {}
                    Component::ParentDir => {
                        base.pop();
                    }
                    other => base.push(other.as_os_str()),
                }
            }
            return base;
        }
        if let Some(name) = cur.components().next_back() {
            missing_tail.push(name);
        }
        match cur.parent() {
            Some(parent) if !parent.as_os_str().is_empty() && parent != cur => cur = parent,
            _ => return abs,
        }
    }
}

fn dir_is_writable_readonly_check(path: &Path) -> bool {
    let test_bin = if Path::new("/bin/test").is_file() {
        "/bin/test"
    } else if Path::new("/usr/bin/test").is_file() {
        "/usr/bin/test"
    } else {
        return false;
    };
    Command::new(test_bin)
        .arg("-w")
        .arg(path)
        .status()
        .is_ok_and(|s| s.success())
}

#[derive(Debug, Clone)]
pub(crate) struct VolumeDiagInput<'a> {
    pub repo_canon: &'a Path,
    pub wt_logical: &'a Path,
    pub wt_display: &'a Path,
    pub data_logical: &'a Path,
    pub repo_dev: u64,
    pub wt_dev: u64,
    pub home_dev: Option<u64>,
    pub repo_mount: Option<&'a MountEntry>,
    pub wt_mount: Option<&'a MountEntry>,
    pub is_macos: bool,
    pub repo_mount_writable: bool,
}

pub(crate) fn evaluate_cow_volume(input: &VolumeDiagInput<'_>) -> Result<String, String> {
    if input.wt_logical.starts_with(input.repo_canon) {
        return Err(format!(
            "AAF_WORKTREE_DIR ({}) is inside the repository {}; move it outside the repo",
            input.wt_display.display(),
            input.repo_canon.display()
        ));
    }
    let allowed_data_wt = input.data_logical.join("worktrees");
    if input.wt_logical.starts_with(input.data_logical)
        && !input.wt_logical.starts_with(&allowed_data_wt)
    {
        return Err(format!(
            "AAF_WORKTREE_DIR ({}) is inside data dir {} outside {}/worktrees; move it outside data dir or use {}/worktrees",
            input.wt_display.display(),
            input.data_logical.display(),
            input.data_logical.display(),
            input.data_logical.display(),
        ));
    }

    if input.is_macos
        && let Some(rm) = input.repo_mount
        && rm.fs_type != "apfs"
    {
        return Err(format!(
            "repo volume {} ({}) is not APFS; automatic CoW cache seeding is disabled because this volume does not support APFS clones",
            rm.mount_point.display(),
            rm.fs_type
        ));
    }

    let wt_non_apfs = input.is_macos && input.wt_mount.is_some_and(|m| m.fs_type != "apfs");
    if input.repo_dev != input.wt_dev || wt_non_apfs {
        let suggestion = if let Some(rm) = input.repo_mount
            && rm.mount_point != Path::new("/")
            && !rm.read_only
            && input.repo_mount_writable
        {
            format!(
                "export AAF_WORKTREE_DIR={}",
                rm.mount_point.join(".aaf-worktrees").display()
            )
        } else if input.home_dev.is_some_and(|hd| hd == input.repo_dev) {
            "unset AAF_WORKTREE_DIR (use default $HOME/.cache/autofactory/worktrees)".to_string()
        } else if let Some(parent) = input.repo_canon.parent().filter(|p| *p != Path::new("/")) {
            format!(
                "export AAF_WORKTREE_DIR={}",
                parent.join(".aaf-worktrees").display()
            )
        } else {
            "set AAF_WORKTREE_DIR to a writable directory on the same volume as the repo"
                .to_string()
        };
        let why = if wt_non_apfs {
            "worktrees volume is not APFS"
        } else {
            "worktrees root and repo are on different devices"
        };
        return Err(format!(
            "{why} (AAF_WORKTREE_DIR={}); CoW cache seeding will skip; suggestion: {suggestion}",
            input.wt_display.display()
        ));
    }

    Ok(format!(
        "{} shares a CoW-capable volume with the repo",
        input.wt_display.display()
    ))
}

pub(super) fn worktree_cow_volume_check(ctx: &Ctx, repo_root: Option<&Path>) -> Check {
    let Some(root) = repo_root else {
        return not_applicable("worktree_cow_volume");
    };
    let repo_canon = nearest_existing_canonical(root).unwrap_or_else(|_| root.to_path_buf());
    let wt_logical = logical_canonical(&ctx.worktrees_dir, &ctx.cwd);
    let data_logical = logical_canonical(&ctx.data_dir, &ctx.cwd);

    let (Ok(repo_dev), Ok(wt_dev)) = (device_id(&repo_canon), device_id(&wt_logical)) else {
        return advisory(
            "worktree_cow_volume",
            Err("could not inspect device IDs for repo or AAF_WORKTREE_DIR".to_string()),
        );
    };
    let home_dev = device_id(&ctx.home).ok();

    let mounts = if cfg!(target_os = "macos") {
        read_macos_mounts().ok()
    } else {
        None
    };
    let wt_existing =
        nearest_existing_canonical(&wt_logical).unwrap_or_else(|_| wt_logical.clone());
    let repo_mount = mounts
        .as_deref()
        .and_then(|ms| find_mount_for_path(&repo_canon, ms))
        .filter(|m| device_id(&m.mount_point).ok() == Some(repo_dev));
    let wt_mount = mounts
        .as_deref()
        .and_then(|ms| find_mount_for_path(&wt_existing, ms))
        .filter(|m| device_id(&m.mount_point).ok() == Some(wt_dev));
    let repo_mount_writable = repo_mount
        .is_some_and(|rm| !rm.read_only && dir_is_writable_readonly_check(&rm.mount_point));

    let res = evaluate_cow_volume(&VolumeDiagInput {
        repo_canon: &repo_canon,
        wt_logical: &wt_logical,
        wt_display: &ctx.worktrees_dir,
        data_logical: &data_logical,
        repo_dev,
        wt_dev,
        home_dev,
        repo_mount,
        wt_mount,
        is_macos: cfg!(target_os = "macos"),
        repo_mount_writable,
    });
    advisory("worktree_cow_volume", res)
}

pub(super) fn seed_cache_dry_run_check(
    ctx: &Ctx,
    repo_root: Option<&Path>,
    wt_cfg: &WorktreeConfig,
) -> Check {
    let Some(root) = repo_root else {
        return not_applicable("seed_cache_dry_run");
    };
    let seed_val = ctx
        .aaf_env
        .iter()
        .find(|(k, _)| k == SEED_CACHE_ENV)
        .map(|(_, v)| OsStr::new(v.as_str()));
    if !seed_cache_enabled_from(seed_val) {
        return advisory(
            "seed_cache_dry_run",
            Ok("disabled via AAF_SEED_CACHE=0".to_string()),
        );
    }
    let wt_logical = logical_canonical(&ctx.worktrees_dir, &ctx.cwd);
    let res = match dry_run_seed(root, wt_cfg, &wt_logical) {
        Ok(items) if items.is_empty() => Ok("no seedable cache markers at HEAD".to_string()),
        Ok(items) => {
            let summary: Vec<String> = items
                .into_iter()
                .map(|it| {
                    if it.would_seed {
                        format!("would seed {}", it.rel_dir)
                    } else {
                        format!(
                            "skip {} ({})",
                            it.rel_dir,
                            it.reason.as_deref().unwrap_or("unknown")
                        )
                    }
                })
                .collect();
            Ok(summary.join("; "))
        }
        Err(err) => Ok(format!("dry-run unavailable ({err})")),
    };
    advisory("seed_cache_dry_run", res)
}

pub(crate) fn evaluate_android_sdk_service(
    profile: Option<&str>,
    worktree: &WorktreeConfig,
    plist_xml: Option<&str>,
    env_has_sdk: bool,
) -> Option<Result<String, String>> {
    if !matches!(profile, Some("android")) {
        return None;
    }
    let copies_local_props = worktree.copy_files.iter().any(|p| {
        Path::new(p)
            .file_name()
            .is_some_and(|name| name == "local.properties")
    });
    let plist_has_sdk = plist_xml.is_some_and(|xml| {
        xml.contains("<key>ANDROID_HOME</key>") || xml.contains("<key>ANDROID_SDK_ROOT</key>")
    });
    if copies_local_props || plist_has_sdk {
        let via = match (plist_has_sdk, copies_local_props) {
            (true, true) => "LaunchAgent plist + [worktree].copy_files",
            (true, false) => "LaunchAgent plist (ANDROID_HOME/ANDROID_SDK_ROOT)",
            (false, true) => "[worktree].copy_files (local.properties)",
            (false, false) => unreachable!(),
        };
        return Some(Ok(format!(
            "Android SDK available to headless gates via {via}"
        )));
    }
    if plist_xml.is_some() {
        return Some(Err(
            "LaunchAgent plist lacks ANDROID_HOME/ANDROID_SDK_ROOT and [worktree].copy_files has no local.properties; \
             headless Gradle gates may fail with 'SDK location not found' — reinstall service (`aaf ui --install-service`) \
             or add local.properties to [worktree].copy_files"
                .to_string(),
        ));
    }
    if env_has_sdk {
        return Some(Ok(
            "ANDROID_HOME/ANDROID_SDK_ROOT set in shell (no LaunchAgent plist installed)"
                .to_string(),
        ));
    }
    Some(Err(
        "ANDROID_HOME/ANDROID_SDK_ROOT is not set and [worktree].copy_files has no local.properties; \
         worktree Gradle gates may fail with 'SDK location not found'"
            .to_string(),
    ))
}

pub(super) fn android_sdk_service_check(
    ctx: &Ctx,
    root_opt: Option<&Path>,
    profile: Option<&str>,
    worktree: &WorktreeConfig,
) -> Check {
    if root_opt.is_none() {
        return not_applicable("android_service_sdk");
    }
    let plist_path = ctx
        .home
        .join("Library/LaunchAgents/com.autoagentfactory.ui.plist");
    let plist_xml = fs::read_to_string(&plist_path).ok();
    let env_has_sdk = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .iter()
        .any(|k| std::env::var(k).ok().is_some_and(|v| !v.trim().is_empty()));
    match evaluate_android_sdk_service(profile, worktree, plist_xml.as_deref(), env_has_sdk) {
        Some(res) => advisory("android_service_sdk", res),
        None => Check {
            name: "android_service_sdk",
            ok: true,
            required: false,
            applicable: false,
            detail: "not applicable: project profile is not android".to_string(),
        },
    }
}

pub(super) fn worktree_checks(
    ctx: &Ctx,
    root_opt: Option<&Path>,
    profile: Option<&str>,
    worktree: &WorktreeConfig,
) -> Vec<Check> {
    vec![
        worktree_cow_volume_check(ctx, root_opt),
        seed_cache_dry_run_check(ctx, root_opt, worktree),
        android_sdk_service_check(ctx, root_opt, profile, worktree),
    ]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn apfs(mount_point: &str, read_only: bool) -> MountEntry {
        MountEntry {
            mount_point: PathBuf::from(mount_point),
            fs_type: "apfs".to_string(),
            read_only,
        }
    }

    #[test]
    fn doctor_seed_cross_volume_suggests_writable_mount_point() {
        let rm = apfs("/Volumes/Data", false);
        let wm = apfs("/", true);
        let err = evaluate_cow_volume(&VolumeDiagInput {
            repo_canon: Path::new("/Volumes/Data/Toan/my-app"),
            wt_logical: Path::new("/Users/alex/.cache/autofactory/worktrees"),
            wt_display: Path::new("/Users/alex/.cache/autofactory/worktrees"),
            data_logical: Path::new("/Users/alex/.local/share/autofactory"),
            repo_dev: 10,
            wt_dev: 20,
            home_dev: Some(20),
            repo_mount: Some(&rm),
            wt_mount: Some(&wm),
            is_macos: true,
            repo_mount_writable: true,
        })
        .unwrap_err();
        assert!(
            err.contains("export AAF_WORKTREE_DIR=/Volumes/Data/.aaf-worktrees"),
            "{err}"
        );
    }

    #[test]
    fn doctor_seed_root_mount_never_suggests_slash_aaf_worktrees() {
        let root_m = apfs("/", true);
        let ext_m = apfs("/Volumes/Ext", false);
        // Case 1: repo on `/` (sealed read-only macOS root) and $HOME same dev -> unset AAF_WORKTREE_DIR.
        let err1 = evaluate_cow_volume(&VolumeDiagInput {
            repo_canon: Path::new("/Users/alex/src/app"),
            wt_logical: Path::new("/Volumes/Ext/wt"),
            wt_display: Path::new("/Volumes/Ext/wt"),
            data_logical: Path::new("/Users/alex/.local/share/autofactory"),
            repo_dev: 10,
            wt_dev: 20,
            home_dev: Some(10),
            repo_mount: Some(&root_m),
            wt_mount: Some(&ext_m),
            is_macos: true,
            repo_mount_writable: false,
        })
        .unwrap_err();
        assert!(!err1.contains("/.aaf-worktrees"), "{err1}");
        assert!(err1.contains("unset AAF_WORKTREE_DIR"), "{err1}");

        // Case 2: repo on `/` and $HOME on a different dev -> suggest sibling `<parent>/.aaf-worktrees`, not `/.aaf-worktrees`.
        let err2 = evaluate_cow_volume(&VolumeDiagInput {
            repo_canon: Path::new("/srv/repos/app"),
            wt_logical: Path::new("/Volumes/Ext/wt"),
            wt_display: Path::new("/Volumes/Ext/wt"),
            data_logical: Path::new("/Users/alex/.local/share/autofactory"),
            repo_dev: 10,
            wt_dev: 20,
            home_dev: Some(30),
            repo_mount: Some(&root_m),
            wt_mount: Some(&ext_m),
            is_macos: true,
            repo_mount_writable: false,
        })
        .unwrap_err();
        assert!(
            err2.contains("export AAF_WORKTREE_DIR=/srv/repos/.aaf-worktrees"),
            "{err2}"
        );
        assert!(!err2.contains("=/.aaf-worktrees"), "{err2}");
    }

    #[test]
    fn doctor_seed_non_apfs_repo_warns_without_suggesting_that_volume() {
        let hfs = MountEntry {
            mount_point: PathBuf::from("/Volumes/LegacyHFS"),
            fs_type: "hfs".to_string(),
            read_only: false,
        };
        let err = evaluate_cow_volume(&VolumeDiagInput {
            repo_canon: Path::new("/Volumes/LegacyHFS/app"),
            wt_logical: Path::new("/Users/alex/.cache/autofactory/worktrees"),
            wt_display: Path::new("/Users/alex/.cache/autofactory/worktrees"),
            data_logical: Path::new("/Users/alex/.local/share/autofactory"),
            repo_dev: 99,
            wt_dev: 20,
            home_dev: Some(20),
            repo_mount: Some(&hfs),
            wt_mount: None,
            is_macos: true,
            repo_mount_writable: true,
        })
        .unwrap_err();
        assert!(
            err.contains("not APFS") && err.contains("disabled"),
            "{err}"
        );
        assert!(
            !err.contains("export AAF_WORKTREE_DIR=/Volumes/LegacyHFS"),
            "must not suggest pointing AAF_WORKTREE_DIR onto a non-APFS volume: {err}"
        );
    }

    #[test]
    fn doctor_android_sdk_service_warns_when_plist_and_copy_files_both_lack_sdk() {
        let empty = WorktreeConfig::default();
        let plist_no_sdk = "<dict><key>PATH</key><string>/usr/bin</string></dict>";
        let err = evaluate_android_sdk_service(Some("android"), &empty, Some(plist_no_sdk), true)
            .unwrap()
            .unwrap_err();
        assert!(err.contains("ANDROID_HOME"), "{err}");

        let plist_with_sdk = "<dict><key>ANDROID_HOME</key><string>/sdk</string></dict>";
        assert!(
            evaluate_android_sdk_service(Some("android"), &empty, Some(plist_with_sdk), false)
                .unwrap()
                .is_ok()
        );

        let with_copy = WorktreeConfig {
            copy_files: vec!["PhoneConnect/local.properties".to_string()],
            ..Default::default()
        };
        assert!(
            evaluate_android_sdk_service(Some("android"), &with_copy, Some(plist_no_sdk), false)
                .unwrap()
                .is_ok()
        );
        assert!(
            evaluate_android_sdk_service(Some("rust"), &empty, Some(plist_no_sdk), false).is_none()
        );
    }
}
