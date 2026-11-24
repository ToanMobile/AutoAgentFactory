//! Docs-only fast path against code that reads docs by directory or glob, and
//! against extension-less runners (audit a13 round 2, R2-L3-1 and P2; round 3,
//! R3-L3-1 and R3-L3-2).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::Duration;

use aaf_domain::config::GateConfig;
use aaf_domain::gate::ChangeKind;
use aaf_engine::gate::changes::Change;
use aaf_engine::gate::docs_fast::is_docs_only_in_repo;

fn repo_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    for (path, body) in files {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, body).unwrap();
    }
    for args in [
        &["init", "-q"][..],
        &["add", "."],
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "i",
        ],
    ] {
        aaf_fp::git(dir.path(), args, &[], Duration::from_secs(30)).unwrap();
    }
    dir
}

fn fast(dir: &Path, doc: &str) -> bool {
    let globs = GateConfig::default().head_lock_globs;
    let change = Change {
        kind: ChangeKind::Modified,
        path: doc.to_string(),
        old_path: None,
    };
    is_docs_only_in_repo(dir, &[change], &globs, &[], Duration::from_secs(30)).unwrap()
}

/// Rows: (tracked file, its body, changed doc, may take the fast path). Each
/// row is its own repo holding the doc and the file.
#[test]
fn docs_read_by_directory_glob_or_runner_are_not_fast() {
    let rows: &[(&str, &str, &str, bool)] = &[
        // A test globs the doc's directory: the file name is never spelled.
        (
            "tests/test_docs.py",
            "import pathlib\n\ndef test_docs():\n    for p in pathlib.Path(\"docs\").glob(\"*.md\"):\n        assert \"TODO\" not in p.read_text()\n",
            "docs/guide.md",
            false,
        ),
        // Same, with the directory alone named (no extension token).
        (
            "tests/walk.rs",
            "#[test]\nfn walk() {\n    for e in std::fs::read_dir(\"docs/\").unwrap() {\n        let _ = e;\n    }\n}\n",
            "docs/guide.md",
            false,
        ),
        // An extension-less runner reads the doc by name.
        (
            "Makefile",
            "test:\n\tgrep -q '^## Usage' docs/usage.md\n",
            "docs/usage.md",
            false,
        ),
        (
            "justfile",
            "check:\n    grep -q x docs/usage.md\n",
            "docs/usage.md",
            false,
        ),
        (
            "build/rules.mk",
            "lint:\n\tcat docs/usage.md\n",
            "docs/usage.md",
            false,
        ),
        (
            "GNUmakefile",
            "t:\n\tcat docs/usage.md\n",
            "docs/usage.md",
            false,
        ),
        // Only the code scan hits (the embed scan finds no marker): P2 must
        // still see it when both scans run at once.
        (
            "src/load.py",
            "TEXT = open('NOTES.md').read()\n",
            "NOTES.md",
            false,
        ),
        // The extension spelled without its dot (audit a13 R3-L3-1): a Rust
        // root walk comparing `extension()`, a brace glob.
        (
            "src/walk.rs",
            "pub fn is_doc(p: &std::path::Path) -> bool {\n    p.extension().is_some_and(|e| e == \"md\")\n}\n",
            "README.md",
            false,
        ),
        (
            "scripts/check.mjs",
            "import { globSync } from 'glob';\nfor (const f of globSync('**/*.{md,mdx}')) check(f);\n",
            "docs/guide.md",
            false,
        ),
        // More runners, and a runner naming the bare directory (R3-L3-2).
        (
            "Rakefile",
            "task :check do\n  sh 'grep -q x docs/usage.md'\nend\n",
            "docs/usage.md",
            false,
        ),
        (
            "lib/tasks/docs.rake",
            "task :d do\n  sh 'cat docs/usage.md'\nend\n",
            "docs/usage.md",
            false,
        ),
        (
            "Makefile.toml",
            "[tasks.check]\nscript = \"grep -q x docs/usage.md\"\n",
            "docs/usage.md",
            false,
        ),
        (
            "Taskfile.yml",
            "tasks:\n  check:\n    cmds:\n      - grep -q x docs/usage.md\n",
            "docs/usage.md",
            false,
        ),
        (
            "Taskfile.yaml",
            "tasks:\n  check:\n    cmds:\n      - cat docs/usage.md\n",
            "docs/usage.md",
            false,
        ),
        (
            "Makefile",
            "lint:\n\tmarkdownlint docs\n",
            "docs/guide.md",
            false,
        ),
        // A bare directory word counts in runners only, not in code.
        (
            "src/lib.rs",
            "pub fn docs() -> u8 {\n    1\n}\n",
            "docs/guide.md",
            true,
        ),
        // Unrelated code and a runner that names other things stay fast.
        (
            "src/lib.rs",
            "pub fn docs_url() -> &'static str {\n    \"https://example.invalid/mydocs\"\n}\n",
            "docs/guide.md",
            true,
        ),
        ("Makefile", "test:\n\tcargo test\n", "docs/usage.md", true),
    ];
    let wrong: Vec<_> = rows
        .iter()
        .filter(|(path, body, doc, want)| {
            let repo = repo_with(&[(path, body), (doc, "# d\n")]);
            fast(repo.path(), doc) != *want
        })
        .collect();
    assert!(wrong.is_empty(), "wrong verdict: {wrong:#?}");
}
