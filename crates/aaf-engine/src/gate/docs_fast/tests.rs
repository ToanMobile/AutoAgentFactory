//! Unit tests of the docs-only fast path in `gate::docs_fast`.

use aaf_domain::config::GateConfig;
use aaf_domain::gate::ChangeKind;

use super::*;

fn ch(kind: ChangeKind, path: &str, old: Option<&str>) -> Change {
    Change {
        kind,
        path: path.to_string(),
        old_path: old.map(str::to_string),
    }
}

#[test]
fn docs_only_accepts_plain_markdown_and_rejects_empty_and_code() {
    let globs = GateConfig::default().head_lock_globs;
    assert!(!is_docs_only(&[], &globs, &[]).unwrap());
    assert!(
        is_docs_only(
            &[
                ch(ChangeKind::Modified, "plan/Audit,Review.md", None),
                ch(ChangeKind::Added, "docs/notes.rst", None),
            ],
            &globs,
            &[],
        )
        .unwrap()
    );
    assert!(
        !is_docs_only(
            &[
                ch(ChangeKind::Modified, "README.md", None),
                ch(ChangeKind::Modified, "src/main.rs", None),
            ],
            &globs,
            &[],
        )
        .unwrap()
    );
}

#[test]
fn docs_only_blocks_agent_config_assets_tests_requirements_and_code_renames() {
    let globs = GateConfig::default().head_lock_globs;
    for blocked in [
        ch(ChangeKind::Modified, "AGENTS.md", None),
        ch(ChangeKind::Modified, "CLAUDE.md", None),
        ch(ChangeKind::Added, ".claude/skills/x/SKILL.md", None),
        ch(ChangeKind::Added, "Assets/Design/note.md", None),
        ch(ChangeKind::Modified, "app/src/main/res/raw/help.txt", None),
        ch(ChangeKind::Modified, "tests/fixtures/expected.txt", None),
        ch(ChangeKind::Modified, "requirements.txt", None),
        ch(ChangeKind::Modified, "CMakeLists.txt", None),
        ch(ChangeKind::Renamed, "docs/new.md", Some("src/Old.kt")),
    ] {
        assert!(
            !is_docs_only(std::slice::from_ref(&blocked), &globs, &[]).unwrap(),
            "must NOT take docs fast path for {blocked:?}"
        );
    }
    assert!(
        !is_docs_only(
            &[ch(ChangeKind::Modified, "docs/critical.md", None)],
            &globs,
            &["docs/critical.md".to_string()],
        )
        .unwrap()
    );
}

/// Temp repo committing `files`. Fixtures spell most macro names from
/// pieces or placeholders, so few lines of this source read as embeds.
fn repo_with(files: &[(&str, String)]) -> tempfile::TempDir {
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

fn in_repo(dir: &Path, path: &str) -> bool {
    let globs = GateConfig::default().head_lock_globs;
    let change = ch(ChangeKind::Modified, path, None);
    is_docs_only_in_repo(dir, &[change], &globs, &[], Duration::from_secs(30)).unwrap()
}

#[test]
fn docs_in_repo_blocks_embedded_docs_and_keeps_plain_docs_fast() {
    let (inc, dt) = (concat!("include", "_str!"), concat!("doc", "test"));
    let repo = repo_with(&[
        ("README.md", "# r\n".into()),
        ("docs/guide.md", "# g\n".into()),
        ("book/ch1.rst", "c\n".into()),
        ("src/lib.rs", format!("#![doc = {inc}(\"../README.md\")]\n")),
        (
            "src/data.rs",
            format!("const D: &str = {inc}(\"notes.txt\");\n"),
        ),
        ("pytest.ini", format!("addopts = --{dt}-glob=*.rst\n")),
    ]);
    assert!(!in_repo(repo.path(), "README.md"), "embedded via macro");
    assert!(!in_repo(repo.path(), "book/ch1.rst"), "test glob target");
    // `.adoc`: `src/lib.rs` spells `md` (R3-L3-1).
    assert!(in_repo(repo.path(), "docs/g.adoc"), "plain doc stays fast");
    assert!(!in_repo(repo.path(), "templates/skills/x/SKILL.md"));
    let not_git = tempfile::TempDir::new().unwrap();
    assert!(!in_repo(not_git.path(), "docs/guide.md"), "fail-closed");
}

/// A test that opens the doc by name at run time depends on it as much
/// as one that embeds it (audit a13 E10-F1). Manifests and site configs
/// that only list a doc leave it fast.
#[test]
fn docs_in_repo_blocks_a_doc_that_code_opens_by_name() {
    let read = "fn guide() {\n    let text = std::fs::read_to_string(format!(\n        \
                \"{}/GUIDE.rst\",\n        env!(\"CARGO_MANIFEST_DIR\")\n    ))\n    .unwrap();\n\
                \x20   assert!(text.contains(\"x\"));\n}\n";
    let repo = repo_with(&[
        ("GUIDE.rst", "# g\n".into()),
        ("README.md", "# r\n".into()),
        ("docs/plain.md", "# p\n".into()),
        ("tests/guide.rs", read.into()),
        ("scripts/check.sh", "grep -q x docs/usage.rst\n".into()),
        ("Cargo.toml", "[package]\nreadme = \"README.md\"\n".into()),
        ("mkdocs.yml", "nav:\n  - plain.md\n".into()),
    ]);
    // Readers name `.rst` docs: code spelling `md` would take every
    // `.md` doc off the fast path (R3-L3-1).
    assert!(!in_repo(repo.path(), "GUIDE.rst"), "read by a test");
    assert!(!in_repo(repo.path(), "docs/usage.rst"), "read by a script");
    assert!(
        in_repo(repo.path(), "README.md"),
        "only a manifest names it"
    );
    assert!(in_repo(repo.path(), "docs/plain.md"), "only a site config");
}

/// Rows: (tracked non-doc body, changed doc, may take fast path), one repo
/// each. `@` and `%` stand for the include-str/include-dir macros.
#[test]
fn docs_in_repo_blocks_wrapped_macro_cased_and_whole_dir_embeds() {
    let rows = [
        ("#![doc = @(\n    \"../docs/g.md\"\n)]", "docs/g.md", false),
        (
            concat!("doc_comment::doc", "test!(\n    \"../g.md\"\n);"),
            "g.md",
            false,
        ),
        (
            "($n:literal) => { @(concat!(\"fx/\", $n)) };",
            "docs/a.md",
            false,
        ),
        ("($p:expr) => { @($p) };", "docs/a.md", false),
        ("($p:expr) => { %($p) };", "docs/a.md", false),
        (
            concat!("doc", "test_glob = [\n    \"*.rst\",\n]"),
            "book/a.rst",
            false,
        ),
        (
            concat!("sphinx-build -b doc", "test src out"),
            "docs/a.md",
            false,
        ),
        (
            concat!("python -m sphinx -b doc", "test src out"),
            "docs/a.md",
            false,
        ),
        (
            concat!("$(SPHINXBUILD) -b doc", "test src out"),
            "docs/a.md",
            false,
        ),
        (
            concat!("extensions = ['sphinx.ext.doc", "test']"),
            "docs/a.md",
            false,
        ),
        (concat!("md", "book  test"), "docs/a.md", false),
        ("#![doc = @(\"../readme.md\")]", "README.md", false),
        (concat!("doc", "test_glob = *.rst"), "docs/GUIDE.RST", false),
        ("static D: Dir = %(\".\");", "README.md", false),
        (
            "static D: Dir = %(\"$CARGO_MANIFEST_DIR\");",
            "docs/g.md",
            false,
        ),
        ("static D: Dir = %(\"docs\");", "docs/g.md", false),
        ("static D: Dir = %(\"./\");", "README.md", false),
        (
            concat!("include_", "bytes!(concat!(\"fx/\", \"b\"))"),
            "docs/a.md",
            false,
        ),
        (
            concat!("doc_comment::doc", "test!(\n    \"../x.txt\"\n);"),
            "g.md",
            false,
        ),
        (
            "#![doc = @(\"../README.md\")]\n\n\n\n\nfn f() {}",
            "docs/g.md",
            true,
        ),
        ("static D: Dir = %(\"web\");", "docs/g.md", true),
        ("#![doc = @(\"../docs/other.md\")]", "docs/g.md", true),
        ("static D: Dir = %(\"mydocs\");", "docs/g.md", true),
    ];
    let wrong: Vec<_> = rows
        .iter()
        .filter(|(body, doc, fast)| {
            let body = body
                .replace('@', concat!("include_", "str!"))
                .replace('%', concat!("include_", "dir!"));
            in_repo(repo_with(&[("t.cfg", body)]).path(), doc) != *fast
        })
        .collect();
    assert!(wrong.is_empty(), "wrong verdict: {wrong:#?}");
}

#[test]
fn docs_in_repo_blocks_go_embed_tab_backtick_and_quoted_wildcards() {
    let marker = concat!("//go:", "embed");
    let rows = [
        (format!("{marker}\tdocs\n"), "docs/g.md", false),
        (format!("{marker} `docs`\n"), "docs/g.md", false),
        (format!("{marker} `docs/sub`\n"), "docs/sub/g.md", false),
        (format!("{marker}\t*\n"), "docs/g.md", false),
        (format!("{marker} \"*\"\n"), "docs/g.md", false),
        (format!("{marker} `*`\n"), "docs/g.md", false),
        (format!("{marker}\tall:*\n"), "docs/g.md", false),
        (format!("{marker} \"all:*\"\n"), "docs/g.md", false),
        (format!("{marker} `all:*`\n"), "docs/g.md", false),
        (format!("{marker}\tweb\n"), "docs/g.md", true),
        (format!("{marker} `web`\n"), "docs/g.md", true),
    ];
    for (body, doc, fast) in rows {
        let repo = repo_with(&[("embed.go", format!("package p\n{body}var f string\n"))]);
        assert_eq!(in_repo(repo.path(), doc), fast, "body={body:?}, doc={doc}");
    }
}
