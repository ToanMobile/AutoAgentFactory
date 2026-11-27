//! Bounded regular-file readers for proof policy files and image evidence.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use super::{MAX_POLICY_FILE_BYTES, MAX_PROOF_IMAGE_BYTES, ProofError, open_canonical_regular};

pub(super) fn read_policy_file(
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

pub(super) fn open_regular_image(
    path: &Path,
    min_bytes: u64,
) -> Result<(PathBuf, File, u64), ProofError> {
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

pub(super) fn read_regular_image(
    path: &Path,
    min_bytes: u64,
) -> Result<(PathBuf, Vec<u8>), ProofError> {
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
