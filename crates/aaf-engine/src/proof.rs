//! Mobile MCP device proof validation and HMAC-signed proof persistence.

#[path = "proof_capture.rs"]
mod capture;
#[path = "proof_png.rs"]
mod png;

pub(crate) use capture::{GateProofCheck, finalize_gate_proof, load_gate_base_capture_cmd};
pub use capture::{
    HOST_CAPTURE_DEVICE, HOST_CAPTURE_TOOL, capture_and_record_proof, captured_proof_image_path,
    load_base_proof_capture_cmd, proof_triggered,
};

use std::env;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Component, Path, PathBuf};

use aaf_domain::config::ProofConfig;
use aaf_domain::proof::{PROOF_SCHEMA_VERSION, ProofRecord};

const MAX_PROOF_IMAGE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_POLICY_FILE_BYTES: u64 = 64 * 1024;
const ALLOWLIST_FILE: &str = ".adb-allowlist";

#[derive(Debug, Clone)]
pub struct ProofInput {
    pub image_path: PathBuf,
    pub source_tool: String,
    pub device_serial: String,
    pub task_id: Option<String>,
    pub now: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedProof {
    pub record: ProofRecord,
    pub proof_path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum ProofError {
    #[error("proof verification failed: {0}")]
    Invalid(String),
    #[error("device denied: {0}")]
    DeniedDevice(String),
    #[error("fingerprint: {0}")]
    Fingerprint(#[from] aaf_fp::FpError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Validates PNG structure, device denylist/allowlist, tool allow-list, and writes an
/// HMAC-signed proof artifact bound to `repo_root` and current `tree_fp`.
pub fn record_proof(
    repo_root: &Path,
    data_dir: &Path,
    cfg: &ProofConfig,
    input: &ProofInput,
) -> Result<RecordedProof, ProofError> {
    let canon_root = fs::canonicalize(repo_root)?;
    validate_tool_and_device(&canon_root, cfg, &input.source_tool, &input.device_serial)?;

    let (canon_img, bytes) = read_regular_image(&input.image_path, cfg.min_bytes)?;
    let (width, height) = png::validate_png(&bytes, cfg.min_width, cfg.min_height, cfg.min_bytes)?;
    let tree_fp = aaf_fp::tree_fp(&canon_root, aaf_fp::GIT_TIMEOUT)?;
    let image_sha256 = aaf_fp::sha256_hex(&bytes);

    let record = ProofRecord {
        schema_version: PROOF_SCHEMA_VERSION,
        repo_root: canon_root.to_string_lossy().into_owned(),
        tree_fp,
        image_path: canon_img.to_string_lossy().into_owned(),
        image_sha256,
        width,
        height,
        byte_len: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        source_tool: input.source_tool.trim().to_string(),
        device_serial: input.device_serial.trim().to_string(),
        task_id: input.task_id.clone(),
        recorded_at: input.now,
    };
    let proof_path = aaf_fp::write_proof(data_dir, &record)?;
    Ok(RecordedProof { record, proof_path })
}

/// Verifies the signed proof artifact for `repo_root` and current (or expected)
/// `tree_fp`, re-checking HMAC signature, device policy, tool policy, and live image SHA-256.
pub fn verify_proof(
    repo_root: &Path,
    data_dir: &Path,
    cfg: &ProofConfig,
    expected_tree_fp: Option<&str>,
) -> Result<ProofRecord, ProofError> {
    let canon_root = fs::canonicalize(repo_root)?;
    let tree_fp = resolve_tree_fp(&canon_root, expected_tree_fp)?;
    verify_proof_inner(&canon_root, &canon_root, data_dir, cfg, &tree_fp)
}

/// Verifies a signed proof bound to `proof_repo_root` (which may be a cleaned-up
/// worktree path that no longer exists on disk) while reading repository
/// denylist/allowlist policy from `policy_repo_root`.
pub fn verify_proof_for_archived_root(
    proof_repo_root: &Path,
    policy_repo_root: &Path,
    data_dir: &Path,
    cfg: &ProofConfig,
    expected_tree_fp: Option<&str>,
) -> Result<ProofRecord, ProofError> {
    let proof_root = canonicalize_archived_root(proof_repo_root);
    let policy_root = fs::canonicalize(policy_repo_root)?;
    let tree_fp = resolve_tree_fp(&proof_root, expected_tree_fp)?;
    verify_proof_inner(&proof_root, &policy_root, data_dir, cfg, &tree_fp)
}

fn canonicalize_archived_root(path: &Path) -> PathBuf {
    if let Ok(canon) = fs::canonicalize(path) {
        return canon;
    }
    let mut missing = Vec::new();
    let mut cur = path;
    while let (Some(parent), Some(name)) = (cur.parent(), cur.file_name()) {
        missing.push(name);
        if let Ok(mut canon) = fs::canonicalize(parent) {
            for part in missing.into_iter().rev() {
                canon.push(part);
            }
            return canon;
        }
        cur = parent;
    }
    path.to_path_buf()
}

fn verify_proof_inner(
    proof_root: &Path,
    policy_root: &Path,
    data_dir: &Path,
    cfg: &ProofConfig,
    tree_fp: &str,
) -> Result<ProofRecord, ProofError> {
    let record = aaf_fp::read_proof(data_dir, proof_root, tree_fp)?;

    if record.schema_version != PROOF_SCHEMA_VERSION {
        return Err(ProofError::Invalid(format!(
            "unsupported proof schema_version {}",
            record.schema_version
        )));
    }
    let expected_repo = proof_root.to_string_lossy();
    if record.repo_root != expected_repo || record.tree_fp != tree_fp {
        return Err(ProofError::Invalid(
            "proof does not match repository or tree fingerprint".to_string(),
        ));
    }
    let is_host_capture = record.source_tool == HOST_CAPTURE_TOOL;
    if is_host_capture {
        validate_host_capture_device(policy_root, cfg, &record.device_serial)?;
    } else {
        validate_tool_and_device(policy_root, cfg, &record.source_tool, &record.device_serial)?;
    }
    if record.byte_len < cfg.min_bytes
        || record.width < cfg.min_width
        || record.height < cfg.min_height
    {
        return Err(ProofError::Invalid(
            "proof screenshot is smaller than policy minimum".to_string(),
        ));
    }

    let img_path = Path::new(&record.image_path);
    let (canon_img, file, live_len) = open_regular_image(img_path, cfg.min_bytes)?;
    if is_host_capture {
        let proofs_dir = fs::canonicalize(data_dir.join("proofs")).map_err(|err| {
            ProofError::Invalid(format!("cannot resolve host proof directory: {err}"))
        })?;
        if !canon_img.starts_with(&proofs_dir) {
            return Err(ProofError::Invalid(
                "host-captured proof image must reside under AAF_DATA_DIR/proofs".to_string(),
            ));
        }
    }
    if live_len != record.byte_len {
        return Err(ProofError::Invalid(
            "screenshot bytes changed after proof was recorded".to_string(),
        ));
    }
    let mut bounded = file.take(MAX_PROOF_IMAGE_BYTES + 1);
    let actual_sha = aaf_fp::sha256_reader(&mut bounded)?;
    if actual_sha != record.image_sha256 {
        return Err(ProofError::Invalid(
            "screenshot bytes changed after proof was recorded".to_string(),
        ));
    }
    Ok(record)
}

/// Loads and verifies HMAC signature of a recorded proof artifact.
pub fn show_proof(
    repo_root: &Path,
    data_dir: &Path,
    tree_fp: Option<&str>,
) -> Result<ProofRecord, ProofError> {
    let canon_root = fs::canonicalize(repo_root)?;
    let fp = resolve_tree_fp(&canon_root, tree_fp)?;
    let record = aaf_fp::read_proof(data_dir, &canon_root, &fp)?;
    if record.repo_root != canon_root.to_string_lossy() || record.tree_fp != fp {
        return Err(ProofError::Invalid(
            "proof binding mismatch for repo_root/tree_fp".to_string(),
        ));
    }
    Ok(record)
}

fn resolve_tree_fp(canon_root: &Path, tree_fp: Option<&str>) -> Result<String, ProofError> {
    match tree_fp {
        Some(fp) if !fp.trim().is_empty() => Ok(fp.trim().to_string()),
        _ => Ok(aaf_fp::tree_fp(canon_root, aaf_fp::GIT_TIMEOUT)?),
    }
}

fn validate_tool_and_device(
    canon_root: &Path,
    cfg: &ProofConfig,
    source_tool: &str,
    device_serial: &str,
) -> Result<(), ProofError> {
    let tool = source_tool.trim();
    if tool.is_empty() {
        return Err(ProofError::Invalid("source_tool is empty".to_string()));
    }
    if cfg.allowed_tools.is_empty() {
        return Err(ProofError::Invalid(
            "[proof].allowed_tools is empty (fail-closed)".to_string(),
        ));
    }
    if !cfg.allowed_tools.iter().any(|t| t == tool) {
        return Err(ProofError::Invalid(format!(
            "source_tool {tool:?} is not in [proof].allowed_tools"
        )));
    }
    let serial = device_serial.trim();
    if serial.is_empty() {
        return Err(ProofError::Invalid("device_serial is empty".to_string()));
    }
    if is_denylisted(canon_root, &cfg.denylist_file, serial)? {
        return Err(ProofError::DeniedDevice(format!(
            "serial {serial:?} is listed in {}",
            cfg.denylist_file
        )));
    }
    if is_global_denylisted(serial)? {
        return Err(ProofError::DeniedDevice(format!(
            "serial {serial:?} is listed in global adb-denylist"
        )));
    }
    check_repo_allowlist(canon_root, serial)?;
    check_global_allowlist(serial)?;
    Ok(())
}

fn validate_host_capture_device(
    canon_root: &Path,
    cfg: &ProofConfig,
    device_serial: &str,
) -> Result<(), ProofError> {
    let serial = device_serial.trim();
    if serial.is_empty() {
        return Err(ProofError::Invalid("device_serial is empty".to_string()));
    }
    if is_denylisted(canon_root, &cfg.denylist_file, serial)? {
        return Err(ProofError::DeniedDevice(format!(
            "serial {serial:?} is listed in {}",
            cfg.denylist_file
        )));
    }
    if is_global_denylisted(serial)? {
        return Err(ProofError::DeniedDevice(format!(
            "serial {serial:?} is listed in global adb-denylist"
        )));
    }
    if serial != HOST_CAPTURE_DEVICE {
        check_repo_allowlist(canon_root, serial)?;
        check_global_allowlist(serial)?;
    }
    Ok(())
}

fn matches_denylist(text: &str, serial: &str) -> bool {
    text.lines().any(|line| {
        let entry = line.split('#').next().unwrap_or("").trim();
        !entry.is_empty() && entry.eq_ignore_ascii_case(serial)
    })
}

fn is_denylisted(canon_root: &Path, rel_file: &str, serial: &str) -> Result<bool, ProofError> {
    let trimmed = rel_file.trim();
    if trimmed.is_empty() {
        return Err(ProofError::Invalid(
            "[proof].denylist_file must not be empty".to_string(),
        ));
    }
    let rel = Path::new(trimmed);
    if rel.is_absolute()
        || rel.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ProofError::Invalid(format!(
            "[proof].denylist_file {trimmed:?} must be a relative path inside the repository"
        )));
    }
    let path = canon_root.join(rel);
    Ok(read_policy_file(&path, Some(canon_root), "denylist")?
        .is_some_and(|text| matches_denylist(&text, serial)))
}

fn global_config_dir() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("AAF_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join(".config").join("autofactory"))
}

fn is_global_denylisted(serial: &str) -> Result<bool, ProofError> {
    let Some(cfg_dir) = global_config_dir() else {
        return Ok(false);
    };
    for name in ["adb-denylist", ".adb-denylist"] {
        let path = cfg_dir.join(name);
        if read_policy_file(&path, None, "global denylist")?
            .is_some_and(|text| matches_denylist(&text, serial))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn check_allowlist_text(text: &str, serial: &str, label: &str) -> Result<(), ProofError> {
    let entries: Vec<&str> = text
        .lines()
        .flat_map(|line| {
            line.split('#')
                .next()
                .unwrap_or("")
                .split([',', ';'])
                .map(str::trim)
                .filter(|item| !item.is_empty())
        })
        .collect();
    if entries.is_empty() {
        return Err(ProofError::DeniedDevice(format!(
            "{label} exists but has no valid device entries (fail-closed)"
        )));
    }
    if entries.iter().any(|e| e.eq_ignore_ascii_case(serial)) {
        Ok(())
    } else {
        Err(ProofError::DeniedDevice(format!(
            "serial {serial:?} is not listed in {label}"
        )))
    }
}

fn check_repo_allowlist(canon_root: &Path, serial: &str) -> Result<(), ProofError> {
    let path = canon_root.join(ALLOWLIST_FILE);
    let Some(text) = read_policy_file(&path, Some(canon_root), "allowlist")? else {
        return Ok(());
    };
    check_allowlist_text(&text, serial, ALLOWLIST_FILE)
}

fn check_global_allowlist(serial: &str) -> Result<(), ProofError> {
    let Some(cfg_dir) = global_config_dir() else {
        return Ok(());
    };
    for name in ["adb-allowlist", ".adb-allowlist"] {
        let path = cfg_dir.join(name);
        if let Some(text) = read_policy_file(&path, None, "global allowlist")? {
            check_allowlist_text(&text, serial, &format!("global {name}"))?;
        }
    }
    Ok(())
}

fn open_canonical_regular(
    path: &Path,
    must_stay_under: Option<&Path>,
    label: &str,
) -> Result<Option<(PathBuf, File, u64)>, ProofError> {
    let lmeta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(ProofError::Invalid(format!(
                "cannot inspect {label} {}: {err}",
                path.display()
            )));
        }
    };
    if lmeta.file_type().is_symlink() || !lmeta.is_file() {
        return Err(ProofError::Invalid(format!(
            "{label} {} must be a regular non-symlink file",
            path.display()
        )));
    }
    let canon = match fs::canonicalize(path) {
        Ok(canon) => canon,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(ProofError::Invalid(format!(
                "cannot resolve {label} {}: {err}",
                path.display()
            )));
        }
    };
    if let Some(root) = must_stay_under
        && !canon.starts_with(root)
    {
        return Err(ProofError::Invalid(format!(
            "{label} {} escapes repository root",
            path.display()
        )));
    }
    let file = match File::open(&canon) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(ProofError::Invalid(format!(
                "cannot open {label} {}: {err}",
                path.display()
            )));
        }
    };
    let fmeta = file.metadata()?;
    if !fmeta.is_file() {
        return Err(ProofError::Invalid(format!(
            "{label} {} must be a regular non-symlink file",
            path.display()
        )));
    }
    let cmeta = fs::symlink_metadata(&canon).map_err(|err| {
        ProofError::Invalid(format!("cannot verify {label} {}: {err}", canon.display()))
    })?;
    if cmeta.file_type().is_symlink() || !cmeta.is_file() {
        return Err(ProofError::Invalid(format!(
            "{label} {} must be a regular non-symlink file",
            canon.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if lmeta.dev() != fmeta.dev()
            || lmeta.ino() != fmeta.ino()
            || cmeta.dev() != fmeta.dev()
            || cmeta.ino() != fmeta.ino()
        {
            return Err(ProofError::Invalid(format!(
                "{label} {} changed while opening",
                path.display()
            )));
        }
    }
    Ok(Some((canon, file, fmeta.len())))
}

fn read_policy_file(
    path: &Path,
    must_stay_under: Option<&Path>,
    label: &str,
) -> Result<Option<String>, ProofError> {
    let Some((_, file, _)) = open_canonical_regular(path, must_stay_under, label)? else {
        return Ok(None);
    };
    let mut buf = Vec::new();
    file.take(MAX_POLICY_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|err| {
            ProofError::Invalid(format!("cannot read {label} {}: {err}", path.display()))
        })?;
    if u64::try_from(buf.len()).unwrap_or(u64::MAX) > MAX_POLICY_FILE_BYTES {
        return Err(ProofError::Invalid(format!(
            "{label} {} exceeds {MAX_POLICY_FILE_BYTES} bytes",
            path.display()
        )));
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| ProofError::Invalid(format!("{label} {} is not valid UTF-8", path.display())))
}

fn open_regular_image(path: &Path, min_bytes: u64) -> Result<(PathBuf, File, u64), ProofError> {
    let Some((canon, file, len)) = open_canonical_regular(path, None, "proof image")? else {
        return Err(ProofError::Invalid("proof image is missing".to_string()));
    };
    if len < min_bytes {
        return Err(ProofError::Invalid(format!(
            "proof image {} is {len} bytes, below minimum {min_bytes} bytes",
            path.display()
        )));
    }
    if len > MAX_PROOF_IMAGE_BYTES {
        return Err(ProofError::Invalid(format!(
            "proof image {} exceeds {MAX_PROOF_IMAGE_BYTES} bytes",
            path.display()
        )));
    }
    Ok((canon, file, len))
}

fn read_regular_image(path: &Path, min_bytes: u64) -> Result<(PathBuf, Vec<u8>), ProofError> {
    let (canon, file, len) = open_regular_image(path, min_bytes)?;
    let cap = usize::try_from(len.min(MAX_PROOF_IMAGE_BYTES)).unwrap_or(0);
    let mut bytes = Vec::with_capacity(cap);
    file.take(MAX_PROOF_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let actual_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if actual_len < min_bytes || actual_len > MAX_PROOF_IMAGE_BYTES {
        return Err(ProofError::Invalid(format!(
            "proof image {} size {actual_len} bytes out of policy range",
            path.display()
        )));
    }
    Ok((canon, bytes))
}
