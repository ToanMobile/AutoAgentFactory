#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use aaf_domain::config::{MachineConfig, RepoConfig};
use aaf_domain::gate::GateMode;
use aaf_domain::ports::Clock;
use aaf_engine::gate::{GateRun, GateRunOptions, run_gate_with_options};
use aaf_engine::git::{GhClient, GitError, MergeStrategy, PrCheck, PrInfo};
use aaf_engine::throttle::{SystemProbe, Throttle};
use tempfile::TempDir;

pub struct FixedClock(pub i64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

pub fn calm_machine() -> MachineConfig {
    MachineConfig {
        max_load_percent: 0,
        ..MachineConfig::default()
    }
}

pub fn new_ws() -> (TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    (tmp, root)
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "dev")
        .env("GIT_AUTHOR_EMAIL", "dev@example.invalid")
        .env("GIT_COMMITTER_NAME", "dev")
        .env("GIT_COMMITTER_EMAIL", "dev@example.invalid")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub fn valid_ui_png() -> Vec<u8> {
    fn crc32(kind: &[u8; 4], data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in kind.iter().chain(data.iter()) {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if (c & 1) != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }
    let mut idat = vec![0x78, 0x9C];
    idat.extend((2..9000usize).map(|i| ((i * 37 + 13) % 251) as u8));
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&1080u32.to_be_bytes());
    ihdr.extend_from_slice(&1920u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    for (k, d) in [
        (*b"IHDR", ihdr.as_slice()),
        (*b"IDAT", idat.as_slice()),
        (*b"IEND", &[]),
    ] {
        out.extend_from_slice(&u32::try_from(d.len()).unwrap().to_be_bytes());
        out.extend_from_slice(&k);
        out.extend_from_slice(d);
        out.extend_from_slice(&crc32(&k, d).to_be_bytes());
    }
    out
}

pub fn init_base_repo(repo: &Path, config_toml: &str, files: &[(&str, &str)]) -> String {
    fs::create_dir_all(repo.join(".autofactory")).unwrap();
    fs::create_dir_all(repo.join("src")).unwrap();
    git(repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join(".autofactory/config.toml"), config_toml).unwrap();
    fs::write(repo.join("src/app.txt"), "v1\n").unwrap();
    for (rel, body) in files {
        let p = repo.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, body).unwrap();
    }
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "base"]);
    git(repo, &["rev-parse", "HEAD"])
}

pub fn run_gate_opts(
    data: &Path,
    repo: &Path,
    cfg: &RepoConfig,
    allowlist: Option<&[String]>,
) -> GateRun {
    let machine = calm_machine();
    let probe = SystemProbe::detect();
    let throttle = Throttle::system(&machine, &probe, &|_| {});
    run_gate_with_options(
        data,
        repo,
        cfg,
        GateMode::Scoped,
        &FixedClock(1000),
        &throttle,
        GateRunOptions {
            proof_secret_allowlist: allowlist,
            ..GateRunOptions::default()
        },
    )
    .unwrap()
}

pub fn pass_script(png: &Path, assertion_name: &str, detail: &str) -> String {
    format!(
        "cp '{}' \"$AAF_PROOF_OUT\"\nprintf '%s' '{{\"assertions\":[{{\"name\":\"{assertion_name}\",\"passed\":true,\"detail\":\"{detail}\"}}]}}' > \"$AAF_PROOF_ASSERTIONS\"\n",
        png.display()
    )
}

pub fn base_gate_cfg() -> RepoConfig {
    let mut cfg = RepoConfig::default();
    cfg.commands.test_scoped = "true".to_string();
    cfg.proof.required = true;
    cfg
}

pub struct FakeGh {
    pub head: String,
    pub merges: AtomicUsize,
}

impl GhClient for FakeGh {
    fn view_pr(&self, _: &Path, n: u64) -> Result<PrInfo, GitError> {
        Ok(PrInfo {
            number: n,
            title: "ui".into(),
            url: format!("https://github.com/o/r/pull/{n}"),
            state: "OPEN".into(),
            is_draft: false,
            head_ref_name: "feat/ui".into(),
            head_ref_oid: self.head.clone(),
            base_ref_name: "main".into(),
            mergeable: "MERGEABLE".into(),
            is_cross_repository: false,
            merge_commit: None,
            author: None,
            body: String::new(),
        })
    }

    fn pr_checks(&self, _: &Path, _: u64) -> Result<Vec<PrCheck>, GitError> {
        Ok(Vec::new())
    }

    fn merge_pr(
        &self,
        _: &Path,
        _: u64,
        _: &str,
        _: MergeStrategy,
        _: Option<&str>,
        _: Option<&Path>,
    ) -> Result<(), GitError> {
        self.merges.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
