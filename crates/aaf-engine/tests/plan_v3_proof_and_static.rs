#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Wave 4 Slice 2: Host-side proof capture (`GATE-01`, `V2-SEC-05`),
//! release hidden-file polish (`GATE-08`), and `LAZY-001` precision (`GATE-09`).

use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_domain::config::{MachineConfig, ProofConfig, RepoConfig};
use aaf_domain::gate::{GateMode, GateStatus, Severity};
use aaf_domain::ports::Clock;
use aaf_engine::gate::changes::AddedLines;
use aaf_engine::gate::hidden::is_benign_release_clutter;
use aaf_engine::gate::{
    HiddenPolicy, run_gate_policy, run_gate_reporting, run_gate_with, static_checks,
};
use aaf_engine::proof::{
    HOST_CAPTURE_TOOL, capture_and_record_proof, load_base_proof_capture_cmd, proof_triggered,
    verify_proof,
};
use aaf_engine::throttle::{SystemProbe, Throttle};

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Tester"]);
    fs::write(dir.join("README.md"), "hello\n").unwrap();
    fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
    fs::create_dir_all(dir.join("src/ui")).unwrap();
    fs::write(dir.join("src/ui/App.tsx"), "export const App = () => 1;\n").unwrap();
    fs::create_dir_all(dir.join("src/backend")).unwrap();
    fs::write(dir.join("src/backend/db.rs"), "pub fn ping() -> u8 { 1 }\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn crc32(kind: &[u8], data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in kind.iter().chain(data.iter()) {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn push_chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    let len = u32::try_from(data.len()).unwrap();
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&kind, data).to_be_bytes());
}

fn build_png(width: u32, height: u32, min_total_bytes: usize) -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    push_chunk(&mut out, *b"IHDR", &ihdr);
    let pad_len = min_total_bytes.saturating_sub(out.len() + 24).max(64);
    let mut idat = Vec::with_capacity(pad_len);
    idat.extend_from_slice(&[0x78, 0x9C]);
    for i in 2..pad_len {
        idat.push(((i * 37 + 13) % 251) as u8);
    }
    push_chunk(&mut out, *b"IDAT", &idat);
    push_chunk(&mut out, *b"IEND", &[]);
    out
}

/// What a gate fingerprint can see: status (ignored too), staged modes and
/// every file below the work tree with its mode and size.
fn tree_snapshot(repo: &Path) -> String {
    let status = git(repo, &["status", "--porcelain=v1", "-uall", "--ignored"]);
    let stage = git(repo, &["ls-files", "--stage"]);
    let find = Command::new("find")
        .arg(repo)
        .args(["-path", "*/.git", "-prune", "-o", "-ls"])
        .output()
        .unwrap();
    let files = String::from_utf8_lossy(&find.stdout);
    format!("--- status\n{status}\n--- stage\n{stage}\n--- files\n{files}")
}

fn calm_machine() -> MachineConfig {
    MachineConfig {
        max_load_percent: 0,
        ..Default::default()
    }
}

#[test]
fn proof_triggered_respects_required_and_trigger_globs() {
    let mut cfg = ProofConfig::default();
    assert!(!proof_triggered(&cfg, &["src/ui/App.tsx".to_string()]));

    cfg.required = true;
    assert!(proof_triggered(&cfg, &[]));
    assert!(proof_triggered(&cfg, &["docs/README.md".to_string()]));

    cfg.trigger_globs = vec!["src/ui/**".to_string(), "app/screens/*.tsx".to_string()];
    assert!(!proof_triggered(
        &cfg,
        &[
            "src/backend/db.rs".to_string(),
            "docs/README.md".to_string(),
        ]
    ));
    assert!(proof_triggered(&cfg, &["src/ui/App.tsx".to_string()]));
    assert!(proof_triggered(
        &cfg,
        &[
            "src/backend/db.rs".to_string(),
            "app/screens/Login.tsx".to_string(),
        ]
    ));

    // End-to-end gate check: backend edit skips proof, UI edit requires proof.
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    init_repo(&repo);
    git(&repo, &["checkout", "-q", "-b", "feat/backend"]);
    fs::write(
        repo.join("src/backend/db.rs"),
        "pub fn ping() -> u8 { 2 }\n",
    )
    .unwrap();

    let mut repo_cfg = RepoConfig::default();
    repo_cfg.commands.test_scoped = "true".to_string();
    repo_cfg.proof.required = true;
    repo_cfg.proof.trigger_globs = vec!["src/ui/**".to_string()];

    let clock = FixedClock(100);
    let machine = calm_machine();
    let before = tree_snapshot(&repo);
    let backend_receipt =
        run_gate_with(&data, &repo, &repo_cfg, GateMode::Scoped, &clock, &machine).unwrap();
    assert_eq!(
        backend_receipt.status,
        GateStatus::Clear,
        "{:?}\n=== receipt\n{:#?}\n=== before gate\n{before}\n=== after gate\n{}",
        backend_receipt.reasons,
        backend_receipt,
        tree_snapshot(&repo)
    );

    // Touching a UI file now requires proof and blocks when no capture_cmd is configured.
    fs::write(repo.join("src/ui/App.tsx"), "export const App = () => 2;\n").unwrap();
    let ui_receipt =
        run_gate_with(&data, &repo, &repo_cfg, GateMode::Scoped, &clock, &machine).unwrap();
    assert_eq!(
        ui_receipt.status,
        GateStatus::Blocked,
        "{:?}",
        ui_receipt.reasons
    );
    assert!(
        ui_receipt
            .reasons
            .iter()
            .any(|r| r.contains("proof required:")),
        "{:?}",
        ui_receipt.reasons
    );
}

#[test]
fn host_proof_capture_runs_after_head_lock_uses_base_sha_and_clears_gate() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    let fixture_png = tmp.path().join("fixture.png");
    let capture_marker = tmp.path().join("capture-ran.log");
    let evil_marker = tmp.path().join("evil-ran.log");
    fs::write(&fixture_png, build_png(1080, 1920, 9000)).unwrap();

    init_repo(&repo);
    let capture_cmd = format!(
        "test -z \"${{AAF_DATA_DIR:-}}\" && printf '%s' \"$AAF_PROOF_OUT\" > '{}' && cp '{}' \"$AAF_PROOF_OUT\"",
        capture_marker.display(),
        fixture_png.display()
    );
    fs::create_dir_all(repo.join(".autofactory")).unwrap();
    let base_toml = format!(
        "[commands]\ntest_scoped = \"true\"\n\n[proof]\nrequired = true\ncapture_cmd = {:?}\ntrigger_globs = [\"src/ui/**\"]\n",
        capture_cmd
    );
    fs::write(repo.join(".autofactory/config.toml"), &base_toml).unwrap();
    git(&repo, &["add", ".autofactory/config.toml"]);
    git(
        &repo,
        &["commit", "-q", "-m", "configure base proof capture"],
    );
    let base_sha = git(&repo, &["rev-parse", "HEAD"]);

    // 1. V2-SEC-05: A head-locked edit MUST block BEFORE capture_cmd runs!
    git(&repo, &["checkout", "-q", "-b", "feat/locked"]);
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.2.0\"\n",
    )
    .unwrap();
    fs::write(repo.join("src/ui/App.tsx"), "export const App = () => 9;\n").unwrap();
    git(&repo, &["add", "Cargo.toml", "src/ui/App.tsx"]);
    git(&repo, &["commit", "-q", "-m", "locked edit"]);

    let mut cfg = RepoConfig::default();
    cfg.commands.test_scoped = "true".to_string();
    cfg.proof.required = true;
    cfg.proof.trigger_globs = vec!["src/ui/**".to_string()];
    // Even with a tampered in-memory capture_cmd, base_sha wins and head_lock blocks first.
    cfg.proof.capture_cmd = format!("touch '{}'", evil_marker.display());

    let clock = FixedClock(200);
    let machine = calm_machine();
    let blocked = run_gate_with(&data, &repo, &cfg, GateMode::Scoped, &clock, &machine).unwrap();
    assert_eq!(blocked.status, GateStatus::Blocked, "{:?}", blocked.reasons);
    assert!(
        !capture_marker.exists(),
        "capture_cmd must NOT run when head_lock blocks"
    );
    assert!(
        !evil_marker.exists(),
        "tampered PR capture_cmd must NEVER execute"
    );

    // 2. Clean UI-only edit on a fresh branch: capture_cmd from base_sha runs AFTER commands,
    // writes to temp outside AAF_DATA_DIR, copies PNG into AAF_DATA_DIR/proofs/, and clears gate.
    git(&repo, &["checkout", "-q", "main"]);
    git(&repo, &["checkout", "-q", "-b", "feat/ui"]);
    fs::write(
        repo.join("src/ui/App.tsx"),
        "export const App = () => 42;\n",
    )
    .unwrap();
    // Plant an agent-written reports/proof.png to ensure host capture never reads it.
    fs::create_dir_all(repo.join("reports")).unwrap();
    fs::write(repo.join("reports/proof.png"), b"not-a-png").unwrap();

    assert_eq!(
        load_base_proof_capture_cmd(&repo, &base_sha)
            .unwrap()
            .as_deref(),
        Some(capture_cmd.as_str())
    );

    let probe = SystemProbe::detect();
    let throttle = Throttle::system(&machine, &probe, &|_| {});
    let first =
        run_gate_reporting(&data, &repo, &cfg, GateMode::Scoped, &clock, &throttle).unwrap();
    assert_eq!(
        first.receipt.status,
        GateStatus::Clear,
        "{:?}",
        first.receipt.reasons
    );
    assert!(!first.reused);
    assert!(!evil_marker.exists(), "tampered capture_cmd must never run");
    assert!(
        capture_marker.exists(),
        "base_sha capture_cmd must have run"
    );

    let proof_out_path =
        Path::new(fs::read_to_string(&capture_marker).unwrap().trim()).to_path_buf();
    let canon_data = fs::canonicalize(&data).unwrap();
    assert!(
        !proof_out_path.starts_with(&canon_data),
        "AAF_PROOF_OUT ({}) must be outside AAF_DATA_DIR ({})",
        proof_out_path.display(),
        canon_data.display()
    );
    assert!(
        !proof_out_path.exists(),
        "temp proof output file must be cleaned up after capture"
    );

    let verified = verify_proof(&repo, &data, &cfg.proof, Some(&first.receipt.tree_fp)).unwrap();
    assert_eq!(verified.source_tool, HOST_CAPTURE_TOOL);
    let stored_img = Path::new(&verified.image_path);
    assert!(stored_img.is_file());
    assert!(
        stored_img.starts_with(canon_data.join("proofs")),
        "stored PNG ({}) must live under AAF_DATA_DIR/proofs",
        stored_img.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(stored_img).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "copied proof PNG must have mode 0600");
    }

    // Subsequent gate run on same tree reuses receipt without re-running capture_cmd.
    fs::remove_file(&capture_marker).unwrap();
    let second =
        run_gate_reporting(&data, &repo, &cfg, GateMode::Scoped, &clock, &throttle).unwrap();
    assert!(second.reused);
    assert_eq!(second.receipt.status, GateStatus::Clear);
    assert!(!capture_marker.exists());
}

#[test]
fn host_proof_capture_mirrors_to_common_root_and_rejects_symlink_and_bad_toml() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let wt = tmp.path().join("wt-task");
    let data = tmp.path().join("data");
    let fixture_png = tmp.path().join("fixture.png");
    fs::write(&fixture_png, build_png(1080, 1920, 9000)).unwrap();

    init_repo(&repo);
    let capture_cmd = format!("cp '{}' \"$AAF_PROOF_OUT\"", fixture_png.display());
    fs::create_dir_all(repo.join(".autofactory")).unwrap();
    fs::write(
        repo.join(".autofactory/config.toml"),
        format!(
            "[proof]\nrequired = true\ncapture_cmd = {:?}\n",
            capture_cmd
        ),
    )
    .unwrap();
    git(&repo, &["add", ".autofactory/config.toml"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let base_sha = git(&repo, &["rev-parse", "HEAD"]);

    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "aaf/t1",
            wt.to_str().unwrap(),
        ],
    );
    let fp = aaf_fp::tree_fp(&wt, aaf_fp::GIT_TIMEOUT).unwrap();
    let cfg = ProofConfig {
        required: true,
        ..ProofConfig::default()
    };
    let clock = FixedClock(300);
    capture_and_record_proof(&wt, &data, &base_sha, &cfg, &fp, &clock).unwrap();

    // Both worktree and common repo root verify for the exact same tree_fp.
    assert!(verify_proof(&wt, &data, &cfg, Some(&fp)).is_ok());
    assert!(verify_proof(&repo, &data, &cfg, Some(&fp)).is_ok());

    // Symlink replacement at AAF_PROOF_OUT must fail closed.
    git(&repo, &["checkout", "-q", "-b", "bad-symlink"]);
    let symlink_cmd = format!(
        "rm -f \"$AAF_PROOF_OUT\" && ln -s '{}' \"$AAF_PROOF_OUT\"",
        fixture_png.display()
    );
    fs::write(
        repo.join(".autofactory/config.toml"),
        format!(
            "[proof]\nrequired = true\ncapture_cmd = {:?}\n",
            symlink_cmd
        ),
    )
    .unwrap();
    git(&repo, &["add", ".autofactory/config.toml"]);
    git(&repo, &["commit", "-q", "-m", "symlink capture"]);
    let symlink_sha = git(&repo, &["rev-parse", "HEAD"]);
    assert!(capture_and_record_proof(&repo, &data, &symlink_sha, &cfg, &fp, &clock).is_err());

    // Invalid TOML at base_sha must fail closed.
    fs::write(repo.join(".autofactory/config.toml"), "[proof\nbroken =").unwrap();
    git(&repo, &["add", ".autofactory/config.toml"]);
    git(&repo, &["commit", "-q", "-m", "corrupt toml"]);
    let bad_toml_sha = git(&repo, &["rev-parse", "HEAD"]);
    assert!(load_base_proof_capture_cmd(&repo, &bad_toml_sha).is_err());
}

#[test]
fn release_hidden_policy_allows_benign_clutter_and_blocks_secret_tmp() {
    assert!(is_benign_release_clutter(".DS_Store"));
    assert!(is_benign_release_clutter("src/ui/.DS_Store"));
    assert!(is_benign_release_clutter("notes.txt~"));
    assert!(is_benign_release_clutter(".claude/settings.local.json"));
    assert!(!is_benign_release_clutter(".DS_Store/"));
    assert!(!is_benign_release_clutter("~"));
    assert!(!is_benign_release_clutter(".claude/settings.json"));
    assert!(!is_benign_release_clutter("secret.tmp"));

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    init_repo(&repo);

    let exclude = repo.join(".git/info/exclude");
    fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    fs::write(
        &exclude,
        ".DS_Store\n*~\n.claude/settings.local.json\nsecret.tmp\n",
    )
    .unwrap();

    fs::write(repo.join(".DS_Store"), "finder").unwrap();
    fs::write(repo.join("README.md~"), "backup").unwrap();
    fs::create_dir_all(repo.join(".claude")).unwrap();
    fs::write(repo.join(".claude/settings.local.json"), "{}\n").unwrap();

    let mut cfg = RepoConfig::default();
    cfg.commands.test_full = "true".to_string();
    let machine = calm_machine();
    let probe = SystemProbe::detect();
    let throttle = Throttle::system(&machine, &probe, &|_| {});
    let clock = FixedClock(400);

    let clear = run_gate_policy(
        &data,
        &repo,
        &cfg,
        GateMode::Release,
        &clock,
        &throttle,
        HiddenPolicy::Warn,
    )
    .unwrap();
    assert_eq!(
        clear.receipt.status,
        GateStatus::Clear,
        "{:?}",
        clear.receipt.reasons
    );
    assert!(
        clear
            .receipt
            .reasons
            .iter()
            .any(|r| r.starts_with("advisory: work tree:")),
        "{:?}",
        clear.receipt.reasons
    );

    // Unexpected ignored file MUST still hard-block --release.
    fs::write(repo.join("secret.tmp"), "top-secret\n").unwrap();
    let blocked = run_gate_policy(
        &data,
        &repo,
        &cfg,
        GateMode::Release,
        &clock,
        &throttle,
        HiddenPolicy::Warn,
    )
    .unwrap();
    assert_eq!(
        blocked.receipt.status,
        GateStatus::Blocked,
        "{:?}",
        blocked.receipt.reasons
    );
    assert!(
        blocked
            .receipt
            .reasons
            .iter()
            .any(|r| r.starts_with("work tree:") && r.contains("secret.tmp")),
        "{:?}",
        blocked.receipt.reasons
    );
    assert!(blocked.receipt.commands.is_empty());
}

fn scan_rules(path: &str, lines: &[&str]) -> Vec<(String, Severity)> {
    let added = AddedLines {
        path: path.to_string(),
        lines: lines
            .iter()
            .enumerate()
            .map(|(i, s)| ((i + 1) as u32, (*s).to_string()))
            .collect(),
    };
    static_checks::scan(&[added])
        .unwrap()
        .findings
        .into_iter()
        .map(|f| (f.rule, f.severity))
        .collect()
}

#[test]
fn lazy_001_allows_docs_comments_and_test_assertions_while_blocking_prod_stubs() {
    let todo_macro = concat!("to", "do!()");
    let unimpl_macro = concat!("unimpl", "emented!()");
    let not_impl = concat!("NotImplemented", "Error");

    // 1. Documentation files do not false-positive on code-syntax stubs.
    assert!(
        scan_rules(
            "docs/guide.md",
            &[&format!("Avoid `{todo_macro}` and `{not_impl}`.")]
        )
        .is_empty()
    );
    assert!(scan_rules("README.markdown", &[unimpl_macro, not_impl]).is_empty());
    assert!(scan_rules("docs/notes.txt", &[not_impl]).is_empty());

    // 2. Comment mentions in code files do not false-positive on code-syntax stubs.
    assert!(
        scan_rules(
            "src/lib.rs",
            &[&format!("// replaced {todo_macro} with real impl")]
        )
        .is_empty()
    );
    assert!(scan_rules("src/lib.rs", &[&format!("/* {unimpl_macro} */")]).is_empty());
    assert!(
        scan_rules(
            "src/app.py",
            &[&format!("# raises {not_impl} for abstract methods")]
        )
        .is_empty()
    );

    // 3. Test assertions expecting the unimplemented exception are exempted in test files.
    assert!(
        scan_rules(
            "tests/test_api.py",
            &[
                &format!("with pytest.raises({not_impl}):"),
                &format!("self.assertRaises({not_impl}, adapter.call)"),
                &format!("self.assertRaisesRegex({not_impl}, 'unsupported')"),
            ]
        )
        .is_empty()
    );

    // 4. Bypass attempt in a test file (real stub after assertion) still blocks!
    assert_eq!(
        scan_rules(
            "tests/test_api.py",
            &[&format!("with pytest.raises({not_impl}): raise {not_impl}")]
        ),
        vec![("LAZY-001".to_string(), Severity::Critical)]
    );
    assert_eq!(
        scan_rules("tests/test_api.py", &[&format!("raise {not_impl}")]),
        vec![("LAZY-001".to_string(), Severity::Critical)]
    );

    // 5. Real code-syntax stubs in production code still block as Critical LAZY-001.
    assert_eq!(
        scan_rules("src/pay.rs", &[&format!("pub fn f() {{ {todo_macro} }}")]),
        vec![("LAZY-001".to_string(), Severity::Critical)]
    );
    assert_eq!(
        scan_rules("src/pay.rs", &[&format!("pub fn f() {{ {unimpl_macro} }}")]),
        vec![("LAZY-001".to_string(), Severity::Critical)]
    );
    assert_eq!(
        scan_rules("src/pay.py", &[&format!("raise {not_impl}('stub')")]),
        vec![("LAZY-001".to_string(), Severity::Critical)]
    );

    // 6. Comment-style prose placeholders still trigger LAZY-001.
    assert_eq!(
        scan_rules("docs/plan.md", &[concat!("# TODO: impl", "ement later")]),
        vec![("LAZY-001".to_string(), Severity::Critical)]
    );
}
