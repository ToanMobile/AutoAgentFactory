use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{GateMode, Receipt};
use aaf_domain::ports::Clock;
use tempfile::TempDir;

use crate::throttle::tests::support::Rig;

pub struct FixedClock(pub i64);

impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

/// A fake AWS key id, split so this file holds no matching text.
pub fn fake_secret() -> String {
    ["AKIA", "IOSFODNN7EXAMPLE"].concat()
}

/// A throwaway git repo plus a data dir.
pub struct Repo {
    pub dir: TempDir,
    pub data: TempDir,
}

impl Repo {
    /// An initialised repo with no commit.
    pub fn empty() -> Repo {
        let repo = Repo {
            dir: TempDir::new().unwrap(),
            data: TempDir::new().unwrap(),
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    /// `main` holds one commit with a README, a source file and a test
    /// file; the checked-out branch is `feature`, forked from it.
    pub fn new() -> Repo {
        let repo = Repo::empty();
        repo.write("README.md", "hello\n");
        repo.write("src/lib.rs", "pub fn one() -> u32 {\n    1\n}\n");
        repo.write(
            "tests/a_test.rs",
            "fn checks() {\n    assert_eq!(1, 1);\n}\n",
        );
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "feature"]);
        repo
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    pub fn data_dir(&self) -> &Path {
        self.data.path()
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root().join(rel)
    }

    pub fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(self.root())
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    pub fn write(&self, rel: &str, content: &str) {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    pub fn append(&self, rel: &str, content: &str) {
        let mut text = fs::read_to_string(self.path(rel)).unwrap();
        text.push_str(content);
        fs::write(self.path(rel), text).unwrap();
    }

    pub fn commit(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }

    /// Runs the gate on a calm fake machine: the real entry points read the
    /// real load, which would make every test here wait (up to half an hour)
    /// whenever this machine is busy.
    pub fn gate(&self, cfg: &RepoConfig, mode: GateMode) -> Receipt {
        self.gate_on(&Rig::calm(), cfg, mode, &FixedClock(1_000))
    }

    pub fn gate_on(
        &self,
        rig: &Rig,
        cfg: &RepoConfig,
        mode: GateMode,
        clock: &dyn Clock,
    ) -> Receipt {
        let throttle = rig.throttle();
        super::super::run_gate_throttled(self.data_dir(), self.root(), cfg, mode, clock, &throttle)
            .unwrap()
    }
}

/// Config whose lint and scoped test both succeed without touching the tree.
pub fn passing_cfg() -> RepoConfig {
    let mut cfg = RepoConfig::default();
    cfg.commands.lint = "true".to_string();
    cfg.commands.test_scoped = "true".to_string();
    cfg
}
