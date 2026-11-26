//! Locked paths that are directories on disk (nested repository, gitlink).

use std::fs;
use std::io::Read;
use std::path::Path;

use crate::gate::GateError;

const MAX_DIR_ENTRIES: usize = 10_000;
const MAX_DIR_BYTES: u64 = 64 * 1024 * 1024;

pub(super) fn hash_dir(root: &Path, path: &str) -> Result<String, GateError> {
    let err = |m: String| GateError::Config(format!("locked directory {path} {m}"));
    let mut entries: Vec<(Vec<u8>, &'static str, String)> = Vec::new();
    let mut stack = vec![(root.to_path_buf(), Vec::new())];
    let mut bytes: u64 = 0;
    while let Some((dir, prefix)) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let (name, p) = (entry.file_name(), entry.path());
            // A submodule's `.git` file (`gitdir: ...`) differs per checkout.
            if name == ".git" {
                continue;
            }
            let meta = fs::symlink_metadata(&p)?;
            if entries.len() >= MAX_DIR_ENTRIES {
                return Err(err(format!("exceeds {MAX_DIR_ENTRIES} entries")));
            }
            let mut rel = prefix.clone();
            if !rel.is_empty() {
                rel.push(b'/');
            }
            rel.extend_from_slice(name.as_encoded_bytes());
            if meta.is_symlink() {
                let t = fs::read_link(&p)?;
                let h = aaf_fp::sha256_hex(t.as_os_str().as_encoded_bytes());
                entries.push((rel, "link", format!("link:{h}")));
            } else if meta.is_file() {
                let rem = MAX_DIR_BYTES.saturating_sub(bytes);
                let over = || err(format!("exceeds {MAX_DIR_BYTES} bytes"));
                if meta.len() > rem {
                    return Err(over());
                }
                let mut lim = fs::File::open(&p)?.take(rem + 1);
                let digest = aaf_fp::sha256_reader(&mut lim)?;
                let read = (rem + 1) - lim.limit();
                if read > rem {
                    return Err(over());
                }
                bytes += read;
                entries.push((rel, "file", digest));
            } else if meta.is_dir() {
                entries.push((rel.clone(), "dir", String::new()));
                stack.push((p, rel));
            } else {
                return Err(err("contains unsupported entry".into()));
            }
        }
    }
    entries.sort();
    let mut mat = Vec::new();
    for (rel, kind, digest) in &entries {
        for f in [rel.as_slice(), kind.as_bytes(), digest.as_bytes()] {
            mat.extend_from_slice(f);
            mat.push(0);
        }
        mat.push(b'\n');
    }
    Ok(format!("dir:{}", aaf_fp::sha256_hex(&mat)))
}
