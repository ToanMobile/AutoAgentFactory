//! Embeds git commit, date, and monotonic build number into `AAF_BUILD_VERSION`
//! so `aaf --version` and `aaf doctor` always identify the exact installed binary
//! without breaking SemVer parsing of `CARGO_PKG_VERSION`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn emit_git_rerun_hints(repo: &Path) {
    let head = repo.join(".git/HEAD");
    if head.is_file() {
        println!("cargo:rerun-if-changed={}", head.display());
        if let Ok(contents) = fs::read_to_string(&head)
            && let Some(ref_path) = contents.trim().strip_prefix("ref:")
        {
            let target = repo.join(".git").join(ref_path.trim());
            if target.is_file() {
                println!("cargo:rerun-if-changed={}", target.display());
            }
        }
    }
    let packed = repo.join(".git/packed-refs");
    if packed.is_file() {
        println!("cargo:rerun-if-changed={}", packed.display());
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=AAF_BUILD_NUMBER");
    let pkg = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from);
    let repo = manifest_dir
        .as_deref()
        .and_then(|d| d.parent())
        .and_then(|d| d.parent())
        .map(PathBuf::from);

    let Some(repo) = repo else {
        println!("cargo:rustc-env=AAF_BUILD_VERSION={pkg}");
        return;
    };
    emit_git_rerun_hints(&repo);

    let sha = git(&repo, &["rev-parse", "--short", "HEAD"]);
    let date = git(&repo, &["show", "-s", "--format=%cs", "HEAD"]);
    let dirty = git(&repo, &["status", "--porcelain", "--untracked-files=no"]).is_some();

    let build_tag = env::var("AAF_BUILD_NUMBER")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()))
        .map(|n| format!("b{n}"))
        .or_else(|| git(&repo, &["rev-list", "--count", "HEAD"]).map(|n| format!("r{n}")));

    let mut meta_parts = Vec::new();
    if let Some(tag) = build_tag {
        meta_parts.push(tag);
    }
    if let Some(sha) = sha {
        meta_parts.push(sha);
    }
    if dirty {
        meta_parts.push("dirty".to_string());
    }

    let mut version = pkg;
    if !meta_parts.is_empty() {
        version.push('+');
        version.push_str(&meta_parts.join("."));
    }
    if let Some(date) = date {
        version.push_str(&format!(" ({date})"));
    }
    println!("cargo:rustc-env=AAF_BUILD_VERSION={version}");
}
