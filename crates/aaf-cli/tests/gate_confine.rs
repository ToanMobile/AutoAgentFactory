//! End-to-end confinement tests for gate commands (`aaf gate`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cli_support.rs"]
mod support;

#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::fs;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::process::Stdio;
#[cfg(target_os = "macos")]
use std::thread;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

#[cfg(any(target_os = "macos", target_os = "linux"))]
use serde_json::json;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use support::{Sandbox, calm_machine, memory_is_calm, one_object, write};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use tempfile::TempDir;

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn toml_basic_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn setup_gate_repo(lint: Option<&str>, test_scoped: &str) -> (Sandbox, TempDir, PathBuf) {
    let sb = Sandbox::new();
    calm_machine(&sb, 1);
    let mut cfg = String::from("[commands]\n");
    if let Some(lint_cmd) = lint {
        cfg.push_str(&format!("lint = {}\n", toml_basic_string(lint_cmd)));
    }
    cfg.push_str(&format!(
        "test_scoped = {}\n",
        toml_basic_string(test_scoped)
    ));
    write(&sb.repo.join(".autofactory/config.toml"), &cfg);
    sb.commit_all("config");
    let out_tmp = TempDir::new().unwrap();
    let out_dir = fs::canonicalize(out_tmp.path()).unwrap();
    (sb, out_tmp, out_dir)
}

#[cfg(target_os = "macos")]
#[test]
fn gate_a_test_scoped_cannot_read_receipt_key() {
    if !memory_is_calm("gate_a_test_scoped_cannot_read_receipt_key") {
        return;
    }
    let (sb, _out_tmp, out_dir) = setup_gate_repo(
        None,
        r#"cp "$AAF_DATA_DIR/receipt.key" "$OUT/leak" 2>/dev/null; exit 0"#,
    );
    let out = sb
        .command(&["gate", "--json"])
        .env("OUT", &out_dir)
        .output()
        .unwrap();
    let code = out.status.code().unwrap();
    let val = one_object(&out);
    assert_eq!(code, 0, "{val}");
    assert_eq!(val["status"], json!("CLEAR"), "{val}");
    assert!(
        !out_dir.join("leak").exists(),
        "test_scoped leaked receipt.key into {}",
        out_dir.display()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn gate_b_test_scoped_cannot_write_approvals() {
    if !memory_is_calm("gate_b_test_scoped_cannot_write_approvals") {
        return;
    }
    let (sb, _out_tmp, out_dir) = setup_gate_repo(
        None,
        r#"mkdir -p "$AAF_DATA_DIR/approvals"; echo x > "$AAF_DATA_DIR/approvals/forged"; exit 0"#,
    );
    let out = sb
        .command(&["gate", "--json"])
        .env("OUT", &out_dir)
        .output()
        .unwrap();
    let code = out.status.code().unwrap();
    let val = one_object(&out);
    assert_eq!(code, 0, "{val}");
    assert_eq!(val["status"], json!("CLEAR"), "{val}");
    assert!(
        !sb.data.join("approvals/forged").exists(),
        "test_scoped forged approvals file inside AAF_DATA_DIR"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn gate_c_light_lint_cannot_read_receipt_key() {
    if !memory_is_calm("gate_c_light_lint_cannot_read_receipt_key") {
        return;
    }
    let (sb, _out_tmp, out_dir) = setup_gate_repo(
        Some(r#"cp "$AAF_DATA_DIR/receipt.key" "$OUT/leak" 2>/dev/null; exit 0"#),
        "true",
    );
    let out = sb
        .command(&["gate", "--json"])
        .env("OUT", &out_dir)
        .output()
        .unwrap();
    let code = out.status.code().unwrap();
    let val = one_object(&out);
    assert_eq!(code, 0, "{val}");
    assert_eq!(val["status"], json!("CLEAR"), "{val}");
    assert!(
        !out_dir.join("leak").exists(),
        "light lint command leaked receipt.key into {}",
        out_dir.display()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn gate_d_normal_command_reads_repo_and_writes_temp() {
    if !memory_is_calm("gate_d_normal_command_reads_repo_and_writes_temp") {
        return;
    }
    let (sb, _out_tmp, out_dir) = setup_gate_repo(None, r#"cat README.md > "$OUT/ok""#);
    let out = sb
        .command(&["gate", "--json"])
        .env("OUT", &out_dir)
        .output()
        .unwrap();
    let code = out.status.code().unwrap();
    let val = one_object(&out);
    assert_eq!(code, 0, "{val}");
    assert_eq!(val["status"], json!("CLEAR"), "{val}");
    let ok_path = out_dir.join("ok");
    assert!(ok_path.is_file(), "expected {} to exist", ok_path.display());
    assert_eq!(fs::read_to_string(&ok_path).unwrap(), "fixture\n");
}

#[cfg(target_os = "macos")]
#[test]
fn gate_e_nested_aaf_slot_completes_with_single_slot() {
    if !memory_is_calm("gate_e_nested_aaf_slot_completes_with_single_slot") {
        return;
    }
    let (sb, _out_tmp, _out_dir) = setup_gate_repo(None, r#""$AAF_BIN" slot -- true"#);
    let mut child = sb
        .command(&["gate", "--json"])
        .env("AAF_BIN", env!("CARGO_BIN_EXE_aaf"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("nested `aaf slot -- true` inside confined gate timed out after 30s");
        }
        thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output().unwrap();
    let code = out.status.code().unwrap();
    let val = one_object(&out);
    assert_eq!(code, 0, "{val}");
    assert_eq!(val["status"], json!("CLEAR"), "{val}");
}

#[cfg(target_os = "macos")]
#[test]
fn gate_f_cannot_rename_data_dir_parent() {
    if !memory_is_calm("gate_f_cannot_rename_data_dir_parent") {
        return;
    }
    let data_tmp = TempDir::new().unwrap();
    let data_root = fs::canonicalize(data_tmp.path()).unwrap();
    let p = data_root.join("p");
    let p2 = data_root.join("p2");
    let data_dir = p.join("data");
    fs::create_dir_all(&p).unwrap();
    let cmd = format!(
        r#"mv "{}" "{}" && cp "{}/data/receipt.key" "$OUT/leak"; exit 0"#,
        p.display(),
        p2.display(),
        p2.display()
    );
    let (sb, _out_tmp, out_dir) = setup_gate_repo(None, &cmd);
    let out = sb
        .command(&["gate", "--json"])
        .env("AAF_DATA_DIR", &data_dir)
        .env("OUT", &out_dir)
        .output()
        .unwrap();
    assert!(
        !out_dir.join("leak").exists(),
        "renamed parent directory leaked receipt.key"
    );
    assert!(p.is_dir(), "data dir parent {} was moved away", p.display());
    assert!(!p2.exists(), "renamed dir {} must not exist", p2.display());
    let code = out.status.code().unwrap();
    let val = one_object(&out);
    assert_eq!(code, 0, "{val}");
    assert_eq!(val["status"], json!("CLEAR"), "{val}");
}

#[cfg(target_os = "macos")]
#[test]
fn gate_g_cannot_replace_symlink_data_dir() {
    use std::os::unix::fs::symlink;

    if !memory_is_calm("gate_g_cannot_replace_symlink_data_dir") {
        return;
    }
    let data_tmp = TempDir::new().unwrap();
    let data_root = fs::canonicalize(data_tmp.path()).unwrap();
    let p = data_root.join("p");
    let real = data_root.join("real");
    fs::create_dir_all(&p).unwrap();
    fs::create_dir_all(&real).unwrap();
    let link = p.join("data");
    symlink(&real, &link).unwrap();

    let cmd = format!(
        r#"rm "{}" && mkdir "{}" && echo FORGED > "{}/receipt.key"; exit 0"#,
        link.display(),
        link.display(),
        link.display()
    );
    let (sb, _out_tmp, out_dir) = setup_gate_repo(None, &cmd);
    let out = sb
        .command(&["gate", "--json"])
        .env("AAF_DATA_DIR", &link)
        .env("OUT", &out_dir)
        .output()
        .unwrap();
    let meta = fs::symlink_metadata(&link).expect("symlink data dir missing");
    assert!(
        meta.file_type().is_symlink(),
        "symlink data dir {} was replaced",
        link.display()
    );
    assert_eq!(
        fs::read_link(&link).unwrap(),
        real,
        "symlink target changed"
    );
    let code = out.status.code().unwrap();
    let val = one_object(&out);
    assert_eq!(code, 0, "{val}");
    assert_eq!(val["status"], json!("CLEAR"), "{val}");
}

#[cfg(target_os = "linux")]
#[test]
fn linux_gate_f_cannot_rename_data_dir_parent() {
    let bwrap_ok = std::process::Command::new("bwrap")
        .args(["--dev-bind", "/", "/", "--tmpfs", "/tmp", "--", "/bin/true"])
        .status()
        .is_ok_and(|s| s.success());
    if !bwrap_ok || !memory_is_calm("linux_gate_f_cannot_rename_data_dir_parent") {
        return;
    }
    let data_tmp = TempDir::new().unwrap();
    let data_root = fs::canonicalize(data_tmp.path()).unwrap();
    let p = data_root.join("p");
    let p2 = data_root.join("p2");
    let data_dir = p.join("data");
    fs::create_dir_all(&p).unwrap();
    let cmd = format!(
        r#"mv "{}" "{}" && cp "{}/data/receipt.key" "$OUT/leak"; exit 0"#,
        p.display(),
        p2.display(),
        p2.display()
    );
    let (sb, _out_tmp, out_dir) = setup_gate_repo(None, &cmd);
    let out = sb
        .command(&["gate", "--json"])
        .env("AAF_DATA_DIR", &data_dir)
        .env("OUT", &out_dir)
        .output()
        .unwrap();
    assert!(
        !out_dir.join("leak").exists(),
        "renamed parent directory leaked receipt.key"
    );
    assert!(p.is_dir(), "data dir parent {} was moved away", p.display());
    assert!(!p2.exists(), "renamed dir {} must not exist", p2.display());
    let code = out.status.code().unwrap();
    let val = one_object(&out);
    assert_eq!(code, 0, "{val}");
    assert_eq!(val["status"], json!("CLEAR"), "{val}");
}
