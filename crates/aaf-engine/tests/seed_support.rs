//! Shared mock backend and fixture helpers for `worktree_seed_*` integration tests.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::new_without_default
)]

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use aaf_engine::git::hydration::seed::SeedBackend;
use aaf_engine::git::hydration::stage::CloneSkip;

pub fn write_nested(root: &Path, rel: &str, content: &str) {
    let full = root.join(rel);
    if let Some(p) = full.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(full, content).unwrap();
}

pub fn copy_tree_no_deref(src: &Path, dst: &Path) -> Result<(), CloneSkip> {
    let status = Command::new("/bin/cp")
        .args(["-RP", "--"])
        .arg(src)
        .arg(dst)
        .status()
        .map_err(|e| CloneSkip::after_create(e.to_string()))?;
    if status.success() {
        Ok(())
    } else {
        Err(CloneSkip::after_create("mock cp failed"))
    }
}

/// Configurable cross-platform `SeedBackend` for deterministic macOS & Linux tests.
pub struct MockBackend {
    pub same_dev: bool,
    pub ps_output: Result<String, String>,
    pub delay: Duration,
    /// If true, writes a partial tree to `dst` then returns `Err(CloneSkip("injected_clone_error"))`.
    pub fail_after_partial_write: bool,
    /// If true, chmods `dst.parent()` to `0555` right after cloning so post-clone cleanup fails.
    pub lock_parent_after_clone: bool,
    /// If true, mutates `dst/Manifest.lock` right after cloning to trigger a non-security S2.4 mismatch.
    pub corrupt_manifest_after_clone: bool,
    /// If true, `check_no_nested_mount` returns `Err("nested_mount: mocked")`.
    pub nested_mount_err: bool,
    /// If `Some(reason)`, writes a sentinel file at `dst/sentinel.txt` and returns
    /// `Err(CloneSkip::pre(reason))` (`created = false`) to test caller `!skip.created` handling.
    pub pre_skip_with_sentinel: Option<String>,
    pub same_dev_calls: AtomicUsize,
}

impl Default for MockBackend {
    fn default() -> Self {
        Self {
            same_dev: true,
            ps_output: Ok("100 /sbin/launchd\n".to_string()),
            delay: Duration::ZERO,
            fail_after_partial_write: false,
            lock_parent_after_clone: false,
            corrupt_manifest_after_clone: false,
            nested_mount_err: false,
            pre_skip_with_sentinel: None,
            same_dev_calls: AtomicUsize::new(0),
        }
    }
}

impl MockBackend {
    pub fn same_dev_call_count(&self) -> usize {
        self.same_dev_calls.load(Ordering::SeqCst)
    }
}

impl SeedBackend for MockBackend {
    fn same_cow_device(&self, _src: &Path, _dst_parent: &Path) -> Result<bool, String> {
        self.same_dev_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.same_dev)
    }

    fn clone_dir_strict(&self, src: &Path, dst: &Path, timeout: Duration) -> Result<(), CloneSkip> {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay.min(timeout));
            if self.delay >= timeout {
                return Err(CloneSkip::pre("timeout"));
            }
        }
        if let Some(reason) = &self.pre_skip_with_sentinel {
            fs::create_dir_all(dst).map_err(|e| CloneSkip::pre(e.to_string()))?;
            fs::write(dst.join("sentinel.txt"), "keep-me")
                .map_err(|e| CloneSkip::pre(e.to_string()))?;
            return Err(CloneSkip::pre(reason.clone()));
        }
        if self.fail_after_partial_write {
            fs::create_dir_all(dst.join("partial"))
                .map_err(|e| CloneSkip::after_create(e.to_string()))?;
            fs::write(dst.join("partial/junk.txt"), "half-written")
                .map_err(|e| CloneSkip::after_create(e.to_string()))?;
            return Err(CloneSkip::after_create("injected_clone_error"));
        }
        copy_tree_no_deref(src, dst)?;
        if self.corrupt_manifest_after_clone {
            let _ = fs::write(dst.join("Manifest.lock"), "DRIFTED\n");
        }
        #[cfg(unix)]
        if self.lock_parent_after_clone
            && let Some(parent) = dst.parent()
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o555));
        }
        Ok(())
    }

    fn clone_entries_strict(
        &self,
        src_dir: &Path,
        entries: &[OsString],
        dst_dir: &Path,
        timeout: Duration,
    ) -> Result<(), CloneSkip> {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay.min(timeout));
            if self.delay >= timeout {
                return Err(CloneSkip::pre("timeout"));
            }
        }
        fs::create_dir(dst_dir).map_err(|e| CloneSkip::pre(e.to_string()))?;
        for e in entries {
            copy_tree_no_deref(&src_dir.join(e), &dst_dir.join(e))?;
        }
        Ok(())
    }

    fn ps_listing(&self, _timeout: Duration) -> Result<String, String> {
        self.ps_output.clone()
    }

    fn check_no_nested_mount(
        &self,
        _src: &Path,
        _sub_entries: Option<&[OsString]>,
        _deadline: Instant,
    ) -> Result<(), String> {
        if self.nested_mount_err {
            Err("nested_mount: mocked".to_string())
        } else {
            Ok(())
        }
    }
}
