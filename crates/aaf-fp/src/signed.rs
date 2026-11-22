//! HMAC-signed receipts, approvals, and proof records with private file permissions.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use aaf_domain::gate::{Approval, GateMode, GateStatus, PostGateAuditRecord, Receipt, StoreStatus};
use aaf_domain::proof::ProofRecord;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::{FpError, hex, repo_id, sha256_hex};

const KEY_FILE: &str = "receipt.key";
const KEY_LEN: usize = 32;

static TEMP_SIGNED_SEQ: AtomicU64 = AtomicU64::new(0);

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let nibble = |c: u8| {
        char::from(c)
            .to_digit(16)
            .and_then(|d| u8::try_from(d).ok())
    };
    text.as_bytes()
        .chunks(2)
        .map(|pair| Some(nibble(pair[0])? << 4 | nibble(pair[1])?))
        .collect()
}

/// Reads the signing key. The hook uses this: a missing key means no valid
/// receipt can exist.
pub fn load_key(data_dir: &Path) -> Result<Vec<u8>, FpError> {
    let key = fs::read(data_dir.join(KEY_FILE)).map_err(|_| FpError::BadKey)?;
    if key.len() < KEY_LEN {
        return Err(FpError::BadKey);
    }
    Ok(key)
}

/// Reads the signing key, creating it (mode 0600) on first use.
pub fn load_or_create_key(data_dir: &Path) -> Result<Vec<u8>, FpError> {
    if let Ok(key) = load_key(data_dir) {
        return Ok(key);
    }
    // The key signs what may merge: its directory is the owner's alone.
    private_dir(data_dir)?;
    let mut key = [0u8; KEY_LEN];
    fs::File::open("/dev/urandom")?.read_exact(&mut key)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(data_dir.join(KEY_FILE)) {
        Ok(mut file) => {
            file.write_all(&key)?;
            Ok(key.to_vec())
        }
        // Another process created it first: use theirs.
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => load_key(data_dir),
        Err(err) => Err(err.into()),
    }
}

/// A JSON payload and the HMAC-SHA256 of its exact bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signed {
    pub payload: String,
    pub hmac: String,
}

/// Domain tag of a gate receipt signature. The tag is fed to the HMAC before
/// the payload, so a signature made for one kind of document never verifies
/// as another. The trailing NUL cannot occur at the start of a JSON payload.
pub const RECEIPT_DOMAIN: &str = "receipt\0";
/// Domain tag of a head-lock approval signature.
pub const APPROVAL_DOMAIN: &str = "approval\0";
/// Domain tag of a signed device-verification proof artifact.
pub const PROOF_DOMAIN: &str = "proof\0";
/// Domain tag of a signed post-gate cross-audit verdict.
pub const AUDIT_DOMAIN: &str = "audit\0";

fn mac_for(key: &[u8], domain: &str, payload: &str) -> Result<Hmac<Sha256>, FpError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| FpError::BadKey)?;
    mac.update(domain.as_bytes());
    mac.update(payload.as_bytes());
    Ok(mac)
}

/// Untagged signature. Receipts and approvals do not use it: see
/// [`sign_tagged`].
pub fn sign(key: &[u8], payload: String) -> Result<Signed, FpError> {
    sign_tagged(key, "", payload)
}

/// Constant-time check that `signed.hmac` matches `signed.payload`, untagged.
pub fn verify(key: &[u8], signed: &Signed) -> bool {
    verify_tagged(key, "", signed)
}

/// Signs `payload` for one kind of document ([`RECEIPT_DOMAIN`],
/// [`APPROVAL_DOMAIN`], [`PROOF_DOMAIN`]).
pub fn sign_tagged(key: &[u8], domain: &str, payload: String) -> Result<Signed, FpError> {
    let hmac = hex(&mac_for(key, domain, &payload)?.finalize().into_bytes());
    Ok(Signed { payload, hmac })
}

/// Constant-time check that `signed` was made by [`sign_tagged`] with the
/// same key and `domain`.
pub fn verify_tagged(key: &[u8], domain: &str, signed: &Signed) -> bool {
    let Some(expected) = unhex(&signed.hmac) else {
        return false;
    };
    mac_for(key, domain, &signed.payload).is_ok_and(|mac| mac.verify_slice(&expected).is_ok())
}

pub fn receipt_path(data_dir: &Path, repo_root: &Path) -> PathBuf {
    data_dir
        .join("receipts")
        .join(repo_id(repo_root))
        .join("latest.json")
}

pub fn approval_path(data_dir: &Path, repo_root: &Path, lock_fp: &str) -> PathBuf {
    let safe: String = lock_fp
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    data_dir
        .join("approvals")
        .join(repo_id(repo_root))
        .join(format!("{safe}.json"))
}

/// Creates `dir` and its missing parents for the owner only, and closes an
/// existing `dir` that an earlier version (or the umask) left open.
fn private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        builder.mode(0o700);
        builder.create(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    builder.create(dir)
}

/// Signs `value` and stores it at `path`, somewhere below `data_dir`.
///
/// Receipts and approvals decide what may merge and are nobody else's to
/// read: the file is 0600 and every directory from the data dir down to it
/// is 0700, whatever the umask says and whatever was there before. (This
/// crate cannot use the helper of `aaf-engine`, which depends on it.)
fn write_signed<T: Serialize>(
    data_dir: &Path,
    path: &Path,
    key: &[u8],
    domain: &str,
    value: &T,
) -> Result<(), FpError> {
    let payload = serde_json::to_string(value).map_err(|e| FpError::Json(e.to_string()))?;
    let signed = sign_tagged(key, domain, payload)?;
    let body = serde_json::to_vec_pretty(&signed).map_err(|e| FpError::Json(e.to_string()))?;
    let mut dirs: Vec<&Path> = path
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(data_dir))
        .collect();
    // Outermost first, so a new directory is created inside a closed one.
    dirs.reverse();
    for dir in dirs {
        private_dir(dir)?;
    }
    // Write-then-rename with per-call sequence number and O_EXCL (`create_new`)
    // so concurrent threads in the same process never collide on one temp path.
    let seq = TEMP_SIGNED_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_extension(format!("tmp-{}-{seq}", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    if let Err(err) = file.write_all(&body) {
        drop(file);
        let _ = fs::remove_file(&temp);
        return Err(err.into());
    }
    drop(file);
    if let Err(err) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(err.into());
    }
    Ok(())
}

fn read_signed<T: for<'de> Deserialize<'de>>(
    path: &Path,
    key: &[u8],
    domain: &str,
) -> Result<T, FpError> {
    let body = fs::read(path)?;
    let signed: Signed = serde_json::from_slice(&body).map_err(|e| FpError::Json(e.to_string()))?;
    if !verify_tagged(key, domain, &signed) {
        return Err(FpError::BadSignature);
    }
    serde_json::from_str(&signed.payload).map_err(|e| FpError::Json(e.to_string()))
}

/// Signs and stores the receipt as the latest one for its repo root.
pub fn write_receipt(data_dir: &Path, key: &[u8], receipt: &Receipt) -> Result<PathBuf, FpError> {
    let path = receipt_path(data_dir, Path::new(&receipt.repo_root));
    write_signed(data_dir, &path, key, RECEIPT_DOMAIN, receipt)?;
    Ok(path)
}

/// Loads the latest receipt for `repo_root`; fails unless its signature holds.
pub fn read_receipt(data_dir: &Path, key: &[u8], repo_root: &Path) -> Result<Receipt, FpError> {
    read_signed(&receipt_path(data_dir, repo_root), key, RECEIPT_DOMAIN)
}

pub fn write_approval(
    data_dir: &Path,
    key: &[u8],
    repo_root: &Path,
    approval: &Approval,
) -> Result<PathBuf, FpError> {
    let path = approval_path(data_dir, repo_root, &approval.lock_fp);
    write_signed(data_dir, &path, key, APPROVAL_DOMAIN, approval)?;
    Ok(path)
}

/// `Ok(None)` when no approval file exists; an unsigned or tampered file is
/// an error, never an approval.
pub fn read_approval(
    data_dir: &Path,
    key: &[u8],
    repo_root: &Path,
    lock_fp: &str,
) -> Result<Option<Approval>, FpError> {
    let path = approval_path(data_dir, repo_root, lock_fp);
    if !path.exists() {
        return Ok(None);
    }
    read_signed(&path, key, APPROVAL_DOMAIN).map(Some)
}

/// The rule a store submission from `repo_root` must satisfy: the receipt was
/// issued for this very repository (canonical paths compared, so a receipt
/// copied from another repo's slot is refused) and passes
/// [`check_store_receipt`]. Returns the reason on refusal.
pub fn check_store_receipt_in(
    repo_root: &Path,
    receipt: &Receipt,
    current_tree_fp: &str,
) -> Result<(), String> {
    let here = fs::canonicalize(repo_root)
        .map_err(|e| format!("the repository root cannot be resolved ({e})"))?;
    // A path that no longer resolves cannot be this repository.
    if fs::canonicalize(&receipt.repo_root).ok().as_ref() != Some(&here) {
        return Err("the receipt was issued for another repository".to_string());
    }
    check_store_receipt(receipt, current_tree_fp)
}

/// The content rules of a store receipt, without the repository binding:
/// callers that know the repo root use [`check_store_receipt_in`].
pub fn check_store_receipt(receipt: &Receipt, current_tree_fp: &str) -> Result<(), String> {
    if receipt.tree_fp != current_tree_fp {
        return Err("the work tree changed since the last gate".to_string());
    }
    if receipt.mode != GateMode::Release {
        return Err("the last gate was not a release gate".to_string());
    }
    if receipt.status != GateStatus::Clear {
        return Err(format!("the last gate ended {:?}", receipt.status));
    }
    if receipt.store_compliance.status != StoreStatus::Clear {
        return Err(format!(
            "store compliance is {:?}",
            receipt.store_compliance.status
        ));
    }
    Ok(())
}

/// Storage path for the signed proof bound to `(repo_root, tree_fp)`.
pub fn proof_path(data_dir: &Path, repo_root: &Path, tree_fp: &str) -> PathBuf {
    let mut fp_tag = sha256_hex(tree_fp.as_bytes());
    fp_tag.truncate(24);
    data_dir
        .join("proofs")
        .join(repo_id(repo_root))
        .join(format!("{fp_tag}.proof.json"))
}

/// Signs and stores `record` under `PROOF_DOMAIN` with mode 0600.
pub fn write_proof(data_dir: &Path, record: &ProofRecord) -> Result<PathBuf, FpError> {
    let key = load_or_create_key(data_dir)?;
    let path = proof_path(data_dir, Path::new(&record.repo_root), &record.tree_fp);
    write_signed(data_dir, &path, &key, PROOF_DOMAIN, record)?;
    Ok(path)
}

/// Reads and verifies the HMAC signature of the proof stored for `(repo_root, tree_fp)`.
pub fn read_proof(
    data_dir: &Path,
    repo_root: &Path,
    tree_fp: &str,
) -> Result<ProofRecord, FpError> {
    let key = load_key(data_dir)?;
    let path = proof_path(data_dir, repo_root, tree_fp);
    read_signed(&path, &key, PROOF_DOMAIN)
}

/// Storage path for a signed post-gate audit verdict bound to `(repo_root, tree_fp, config_hash)`.
pub fn audit_verdict_path(
    data_dir: &Path,
    repo_root: &Path,
    tree_fp: &str,
    config_hash: &str,
) -> PathBuf {
    let tag_input = format!("{tree_fp}\0{config_hash}");
    let mut tag = sha256_hex(tag_input.as_bytes());
    tag.truncate(24);
    data_dir
        .join("audits")
        .join(repo_id(repo_root))
        .join(format!("{tag}.audit.json"))
}

/// Signs and stores `record` under [`AUDIT_DOMAIN`] with mode 0600.
pub fn write_audit_verdict(
    data_dir: &Path,
    key: &[u8],
    repo_root: &Path,
    record: &PostGateAuditRecord,
) -> Result<PathBuf, FpError> {
    let path = audit_verdict_path(data_dir, repo_root, &record.tree_fp, &record.config_hash);
    write_signed(data_dir, &path, key, AUDIT_DOMAIN, record)?;
    Ok(path)
}

/// `Ok(None)` when no signed post-gate verdict exists; tampered or mismatched binding fails closed.
pub fn read_audit_verdict(
    data_dir: &Path,
    key: &[u8],
    repo_root: &Path,
    tree_fp: &str,
    config_hash: &str,
) -> Result<Option<PostGateAuditRecord>, FpError> {
    let path = audit_verdict_path(data_dir, repo_root, tree_fp, config_hash);
    if !path.exists() {
        return Ok(None);
    }
    let rec: PostGateAuditRecord = read_signed(&path, key, AUDIT_DOMAIN)?;
    if rec.tree_fp != tree_fp || rec.config_hash != config_hash {
        return Err(FpError::BadSignature);
    }
    Ok(Some(rec))
}
