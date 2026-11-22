#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};

use aaf_hook::{Decision, HookEnv, decide};
use serde_json::json;

/// A fake repository, data dir, config dir and home under one temp dir.
pub struct Fixture {
    _dir: tempfile::TempDir,
    pub root: PathBuf,
    pub data: PathBuf,
    pub config: PathBuf,
    pub home: PathBuf,
    /// What `git config --list -z` answers; `None` is "git failed".
    pub git_config: RefCell<Option<String>>,
    /// How often the hook asked for it.
    pub config_lookups: Cell<u32>,
}

impl Fixture {
    pub fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let fixture = Fixture {
            root: base.join("repo"),
            data: base.join("data"),
            config: base.join("config"),
            home: base.join("home"),
            git_config: RefCell::new(Some(String::new())),
            config_lookups: Cell::new(0),
            _dir: dir,
        };
        for path in [&fixture.root, &fixture.data, &fixture.config, &fixture.home] {
            fs::create_dir_all(path).unwrap();
        }
        fixture.checkout("feature/x");
        fixture
    }

    /// Points the fake repo's HEAD at `branch`.
    pub fn checkout(&self, branch: &str) {
        fs::create_dir_all(self.root.join(".git")).unwrap();
        fs::write(
            self.root.join(".git/HEAD"),
            format!("ref: refs/heads/{branch}\n"),
        )
        .unwrap();
    }

    /// Stores git aliases, as `git config --list -z` would print them.
    pub fn aliases(&self, pairs: &[(&str, &str)]) {
        let listing = pairs
            .iter()
            .map(|(name, value)| format!("alias.{name}\n{value}\0"))
            .collect();
        *self.git_config.borrow_mut() = Some(listing);
    }

    pub fn write(&self, relative: &str, body: &str) {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    pub fn decide(&self, payload: &serde_json::Value) -> Decision {
        let root = self.root.clone();
        let lookup = move |_: &Path| Some(root.clone());
        let config = |_: &Path| {
            self.config_lookups.set(self.config_lookups.get() + 1);
            self.git_config.borrow().clone()
        };
        let env = HookEnv {
            repo_root: &lookup,
            git_config: &config,
            data_dir: Some(self.data.clone()),
            config_dir: Some(self.config.clone()),
            home: Some(self.home.clone()),
            cwd: None,
        };
        decide(payload.to_string().as_bytes(), &env)
    }

    pub fn bash(&self, command: &str) -> Decision {
        self.bash_in(&self.root, command)
    }

    /// Same, with the call made from `cwd` instead of the repository root.
    pub fn bash_in(&self, cwd: &Path, command: &str) -> Decision {
        let command = self.fill(command);
        self.decide(&json!({
            "tool_name": "Bash",
            "tool_input": {"command": command},
            "cwd": cwd,
        }))
    }

    pub fn file_tool(&self, tool: &str, path: &str) -> Decision {
        self.decide(&json!({
            "tool_name": tool,
            "tool_input": {"file_path": self.fill(path)},
            "cwd": self.root,
        }))
    }

    pub fn fill(&self, text: &str) -> String {
        text.replace("{root}", &self.root.to_string_lossy())
            .replace("{data}", &self.data.to_string_lossy())
            .replace("{home}", &self.home.to_string_lossy())
    }
}

/// The rule that denied, or `None` when allowed.
pub fn rule(decision: &Decision) -> Option<&'static str> {
    match decision {
        Decision::Allow => None,
        Decision::Deny(denial) => Some(denial.rule),
    }
}

/// Runs every row and reports all mismatches at once.
/// `expected`: `Some(rule)` must be denied by that rule, `None` must be allowed.
pub fn run_corpus(fixture: &Fixture, rows: &[(&str, Option<&str>)]) {
    run_corpus_in(fixture, &fixture.root, rows);
}

/// [`run_corpus`] with every call made from `cwd`.
pub fn run_corpus_in(fixture: &Fixture, cwd: &Path, rows: &[(&str, Option<&str>)]) {
    let mut wrong = Vec::new();
    for (command, expected) in rows {
        let got = fixture.bash_in(cwd, command);
        if rule(&got) != *expected {
            wrong.push(format!("{command:?}: expected {expected:?}, got {got:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} rows wrong:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

/// Same for the file tools: `(tool, path, expected rule)`.
pub fn run_file_corpus(fixture: &Fixture, rows: &[(&str, &str, Option<&str>)]) {
    let mut wrong = Vec::new();
    for (tool, path, expected) in rows {
        let got = fixture.file_tool(tool, path);
        if rule(&got) != *expected {
            wrong.push(format!(
                "{tool} {path:?}: expected {expected:?}, got {got:?}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} rows wrong:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}
