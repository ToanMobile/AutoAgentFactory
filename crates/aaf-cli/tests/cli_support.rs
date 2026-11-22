//! Shared fixture for the `cli_*` integration tests: a throwaway git repo, a
//! fake home and data dir, and a runner for the built `aaf` binary. Included
//! by the other files with `#[path]`; on its own it holds no test.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::new_without_default
)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

pub struct Sandbox {
    _tmp: TempDir,
    pub repo: PathBuf,
    pub home: PathBuf,
    pub data: PathBuf,
    /// Holds a fake `aaf-hook`, first on the child's PATH.
    pub bin: PathBuf,
}

pub fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

pub fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

impl Sandbox {
    /// A git repo with one commit (`README.md`).
    pub fn new() -> Sandbox {
        let sandbox = Sandbox::without_git();
        sandbox.git(&["init", "-q", "-b", "main"]);
        write(&sandbox.repo.join("README.md"), "fixture\n");
        sandbox.commit_all("initial");
        sandbox
    }

    pub fn without_git() -> Sandbox {
        let tmp = TempDir::new().unwrap();
        // macOS temp dirs sit behind a symlink; git reports the real path.
        let root = fs::canonicalize(tmp.path()).unwrap();
        let sandbox = Sandbox {
            repo: root.join("repo"),
            home: root.join("home"),
            data: root.join("data"),
            bin: root.join("bin"),
            _tmp: tmp,
        };
        for dir in [&sandbox.repo, &sandbox.home, &sandbox.bin] {
            fs::create_dir_all(dir).unwrap();
        }
        let hook = sandbox.bin.join("aaf-hook");
        write(&hook, "#!/bin/sh\necho '{}'\n");
        make_executable(&hook);
        sandbox
    }

    fn path_var(&self) -> OsString {
        let mut dirs = vec![self.bin.clone()];
        dirs.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        std::env::join_paths(dirs).unwrap()
    }

    pub fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args([
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=f@example.invalid",
            ])
            .args(args)
            .env("HOME", &self.home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    pub fn commit_all(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }

    /// The `aaf` child process: clean environment, stdin not a terminal.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_aaf"));
        cmd.args(args)
            .current_dir(&self.repo)
            .env_clear()
            .env("HOME", &self.home)
            .env("AAF_DATA_DIR", &self.data)
            .env("PATH", self.path_var())
            .stdin(Stdio::null());
        cmd
    }

    pub fn aaf(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Runs with `--json` and returns (exit code, the one JSON object).
    pub fn json(&self, args: &[&str]) -> (i32, Value) {
        let mut all = args.to_vec();
        all.push("--json");
        let out = self.aaf(&all);
        (out.status.code().unwrap(), one_object(&out))
    }
}

/// Tier 1 of the child (`$HOME/.config/autofactory`): no wait for the CPU
/// load of the machine running the tests, and this many heavy slots. The
/// memory-pressure wait has no switch and stays real (see
/// [`memory_is_calm`]).
pub fn calm_machine(sb: &Sandbox, max_heavy_parallel: u32) {
    write(
        &sb.home.join(".config/autofactory/config.toml"),
        &format!("[machine]\nmax_load_percent = 0\nmax_heavy_parallel = {max_heavy_parallel}\n"),
    );
}

/// False when the machine running the tests is under memory pressure. A
/// heavy command started through the real binary then waits until the
/// pressure eases, with no time limit, and that is the product working as
/// designed: such a test returns early instead of hanging. Unreadable
/// pressure counts as normal, as it does in the product.
///
/// A test that returns early is reported `ok` although it proved nothing,
/// so the skip is made impossible to miss: the `SKIPPED` line is written
/// to the real stderr, which the test harness does not capture (it shows
/// in the output of a passing run), and with `AAF_REQUIRE_REAL=1` the test
/// fails instead. A run that is evidence sets the variable.
pub fn memory_is_calm(test: &str) -> bool {
    use aaf_engine::throttle::{LoadProbe, SystemProbe};
    let require_real = std::env::var_os(REQUIRE_REAL).is_some_and(|value| value == "1");
    calm_or_skip(
        test,
        SystemProbe::detect().pressure(),
        require_real,
        &mut std::io::stderr(),
    )
}

/// The decision of [`memory_is_calm`] for a given pressure reading.
pub fn calm_or_skip(
    test: &str,
    pressure: Option<aaf_engine::throttle::Pressure>,
    require_real: bool,
    notice: &mut dyn std::io::Write,
) -> bool {
    use aaf_engine::throttle::Pressure;
    let pressure = match pressure {
        Some(Pressure::Normal) | None => return true,
        Some(pressure) => pressure,
    };
    assert!(
        !require_real,
        "{REQUIRE_REAL}=1: {test} cannot run for real, memory pressure is {pressure:?}"
    );
    writeln!(
        notice,
        "SKIPPED {test}: memory pressure is {pressure:?}; a heavy command would wait \
         for it to ease. This test proved nothing in this run; {REQUIRE_REAL}=1 \
         makes that a failure."
    )
    .unwrap();
    false
}

/// Environment variable of the test run: `1` turns a skip into a failure.
pub const REQUIRE_REAL: &str = "AAF_REQUIRE_REAL";

/// Asserts stdout is exactly one JSON object on one line and returns it.
pub fn one_object(out: &Output) -> Value {
    let stdout = String::from_utf8(out.stdout.clone()).unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stdout.trim_end().lines().count(),
        1,
        "stdout must be one line, got {stdout:?} (stderr {stderr:?})"
    );
    let value: Value = serde_json::from_str(stdout.trim_end())
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {stdout:?}"));
    assert!(value.is_object(), "not an object: {value}");
    for key in ["ok", "command", "status", "duration_ms"] {
        assert!(value.get(key).is_some(), "missing {key}: {value}");
    }
    value
}
