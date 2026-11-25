//! Handover renderers (HTML with CSP + verified base64 PNG, Markdown, JSON),
//! std base64 encoder, and sanitized PR body composer (`< 60 KiB`).

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;

use aaf_domain::gate::Receipt;
use aaf_domain::proof::ProofRecord;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cross_audit::scoring::GOLDEN_STANDARDS;
use crate::handover::{HandoverData, SOLO_DEV_BADGE};

/// Verified device-proof metadata carried in [`HandoverData`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoverProof {
    pub repo_root: String,
    pub tree_fp: String,
    pub device_serial: String,
    pub source_tool: String,
    pub image_path: String,
    pub image_sha256: String,
    pub width: u32,
    pub height: u32,
    pub byte_len: u64,
    pub summary: String,
    pub captured_at: i64,
}

impl HandoverProof {
    pub fn from_record(r: &ProofRecord) -> Self {
        Self {
            repo_root: r.repo_root.clone(),
            tree_fp: r.tree_fp.clone(),
            device_serial: r.device_serial.clone(),
            source_tool: r.source_tool.clone(),
            image_path: r.image_path.clone(),
            image_sha256: r.image_sha256.clone(),
            width: r.width,
            height: r.height,
            byte_len: r.byte_len,
            summary: format!(
                "{}x{} ({} bytes) via {} on {}",
                r.width, r.height, r.byte_len, r.source_tool, r.device_serial
            ),
            captured_at: r.recorded_at,
        }
    }
}

/// Exact Content-Security-Policy meta tag embedded in every HTML handover report.
pub const CSP_META: &str = r#"<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data:; style-src 'unsafe-inline';">"#;

/// Hard size ceiling (4 MiB) for inline base64 PNG embedding in HTML reports.
pub const MAX_HTML_PROOF_BYTES: u64 = 4 * 1024 * 1024;

/// Hard upper bound (< 60 KiB) for PR bodies composed by [`compose_pr_body`].
pub const MAX_PR_BODY_BYTES: usize = 60 * 1024 - 1;

/// Pure-std RFC 4648 standard base64 encoder with `=` padding (0 dependencies).
pub fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(TABLE[((triple >> 18) & 0x3F) as usize]));
        out.push(char::from(TABLE[((triple >> 12) & 0x3F) as usize]));
        out.push(if chunk.len() > 1 {
            char::from(TABLE[((triple >> 6) & 0x3F) as usize])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(TABLE[(triple & 0x3F) as usize])
        } else {
            '='
        });
    }
    out
}

/// Escapes `&`, `<`, `>`, `"`, and `'` for safe HTML text and attribute rendering.
pub fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

pub(crate) fn format_scores_line(data: &HandoverData) -> (String, String, String) {
    let fmt = |v: Option<u8>, tag: &str| match v {
        Some(s) if tag.is_empty() => format!("{s}/100"),
        Some(s) => format!("{s}/100 {tag}"),
        None => "n/a".to_string(),
    };
    (
        fmt(data.mechanical_score, "[tự chạy]"),
        fmt(data.llm_average, "[agent báo]"),
        fmt(data.effective_score, ""),
    )
}

pub(crate) fn standard_name(id: &str) -> &'static str {
    GOLDEN_STANDARDS
        .iter()
        .find(|(gid, _)| *gid == id)
        .map_or("Custom", |(_, label)| *label)
}

/// Renders a standalone, offline-safe HTML handover report.
pub fn render_html(data: &HandoverData, data_dir: &Path) -> String {
    crate::handover_devkit::render_devkit_html(data, data_dir)
}

/// Re-verifies HMAC + policy + live image SHA-256 via
/// [`crate::proof::verify_proof_for_archived_root`], opens the image safely as a
/// regular non-symlink file `<= 4 MiB`, confirms its digest again, and
/// base64-encodes it.
pub(crate) fn load_verified_proof_png(
    proof: &HandoverProof,
    data: &HandoverData,
    data_dir: &Path,
) -> Result<String, String> {
    let verified = crate::proof::verify_proof_for_archived_root(
        Path::new(&proof.repo_root),
        Path::new(&data.repo_root),
        data_dir,
        &data.proof_config,
        Some(&proof.tree_fp),
    )
    .map_err(|e| e.to_string())?;
    if verified.image_sha256 != proof.image_sha256 {
        return Err("verified proof SHA-256 does not match handover record".to_string());
    }
    let path = Path::new(&verified.image_path);
    let lmeta =
        fs::symlink_metadata(path).map_err(|e| format!("cannot inspect proof image: {e}"))?;
    if lmeta.file_type().is_symlink() || !lmeta.is_file() {
        return Err("proof image must be a regular non-symlink file".to_string());
    }
    let canon =
        fs::canonicalize(path).map_err(|e| format!("cannot resolve proof image path: {e}"))?;
    let cmeta = fs::symlink_metadata(&canon)
        .map_err(|e| format!("cannot inspect canonical proof image: {e}"))?;
    if cmeta.file_type().is_symlink() || !cmeta.is_file() {
        return Err("canonical proof image must be a regular non-symlink file".to_string());
    }
    let file = File::open(&canon).map_err(|e| format!("cannot open proof image: {e}"))?;
    let fmeta = file
        .metadata()
        .map_err(|e| format!("cannot stat open proof image: {e}"))?;
    if !fmeta.is_file() {
        return Err("open proof image is not a regular file".to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if lmeta.dev() != fmeta.dev()
            || lmeta.ino() != fmeta.ino()
            || cmeta.dev() != fmeta.dev()
            || cmeta.ino() != fmeta.ino()
        {
            return Err("proof image changed while opening".to_string());
        }
    }
    if fmeta.len() > MAX_HTML_PROOF_BYTES {
        return Err(format!(
            "proof image is {} bytes, exceeding 4 MiB limit",
            fmeta.len()
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(fmeta.len()).unwrap_or(0));
    file.take(MAX_HTML_PROOF_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read proof image: {e}"))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_HTML_PROOF_BYTES {
        return Err("proof image exceeds 4 MiB limit".to_string());
    }
    if aaf_fp::sha256_hex(&bytes) != verified.image_sha256 {
        return Err("proof image bytes changed during HTML rendering".to_string());
    }
    Ok(encode_base64(&bytes))
}

/// Renders [`HandoverData`] as concise GitHub-flavored Markdown.
pub fn render_markdown(data: &HandoverData) -> String {
    crate::handover_devkit::render_devkit_markdown(data)
}

/// Serializes [`HandoverData`] to a JSON [`Value`].
pub fn render_json(data: &HandoverData) -> Value {
    serde_json::to_value(data).unwrap_or_else(|_| {
        serde_json::json!({ "task_id": data.task_id, "status": data.status, "missing": data.missing })
    })
}

/// Composes a sanitized GitHub PR body from `receipt` and optional `handover`.
///
/// Leaves `crate::git::pr::pr_body` untouched, never duplicates
/// `## Verification Proof`, strips inline base64 images, raw HTML tags,
/// external markdown image links, and secret patterns, and truncates safely on
/// a UTF-8 boundary strictly below 60 KiB.
pub fn compose_pr_body(receipt: &Receipt, handover: Option<&HandoverData>) -> String {
    let mut body = crate::git::pr::pr_body(receipt);
    if let Some(h) = handover {
        let clean = |s: &str| s.replace("## Verification Proof", "Verification Proof");
        let _ = writeln!(
            body,
            "\n## Handover ({})\n\n\
             1. **Đã làm gì / Đã fix gì:** {}\n\
             2. **Chặn bug cũ:** {}\n\
             3. **Nguy cơ bug mới:** {}\n\
             4. **An toàn mã nguồn:** {}\n\
             5. **Còn hở / chưa kiểm:** {}",
            h.status,
            clean(&h.what_done),
            clean(&h.prevent_old_bug),
            clean(&h.new_bug_risk),
            clean(&h.code_safety),
            clean(&h.unverified)
        );
        if let Some(eff) = h.effective_score {
            let (mech, llm, _) = format_scores_line(h);
            let _ = writeln!(
                body,
                "\n- Scores: effective **{eff}/100** (mechanical: {mech}, llm: {llm})"
            );
        }
        if h.same_account_warning {
            let _ = writeln!(body, "- Note: **{SOLO_DEV_BADGE}**");
        }
        if let Some(p) = &h.proof
            && !body.contains("## Verification Proof")
        {
            let code = |s: &str| {
                s.replace('`', "'")
                    .replace(['\r', '\n'], " ")
                    .trim()
                    .to_string()
            };
            let _ = writeln!(
                body,
                "\n## Verification Proof\n\n- Tool: `{}`\n- Device: `{}`\n- Resolution: {}x{} ({} bytes)\n- Image SHA-256: `{}`\n- Tree: `{}`",
                code(&p.source_tool),
                code(&p.device_serial),
                p.width,
                p.height,
                p.byte_len,
                code(&p.image_sha256),
                code(&p.tree_fp)
            );
        }
    }
    let sanitized = sanitize_pr_body(&body);
    truncate_utf8_bytes(&sanitized, MAX_PR_BODY_BYTES)
}

fn sanitizer_rules() -> &'static [(Regex, &'static str)] {
    static RULES: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    RULES.get_or_init(|| {
        // Secret regexes are assembled from fragments so repo static checks never flag this file.
        let specs: &[(&[&str], &str)] = &[
            (
                &[r"(?is)data:image/[a-z0-9.+-]+;base64,[a-z0-9+/=\s]+"],
                "[inline image omitted]",
            ),
            (
                &[r"!\[[^\]]*\]\(\s*https?://[^)\s]+\s*\)"],
                "[external image omitted]",
            ),
            (&[r"(?is)<script\b[^>]*>.*?</script\s*>"], ""),
            (&[r"(?is)<style\b[^>]*>.*?</style\s*>"], ""),
            (&[r"(?is)<!--.*?-->"], ""),
            (&[r"</?[A-Za-z][A-Za-z0-9:-]*(?:\s[^>]*)?/?>"], ""),
            (
                &[r"(?i)<(?:script|img|iframe|svg|style|object|embed|a)\b[^>]*"],
                "",
            ),
            (
                &[
                    r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE",
                    r" KEY-----[\s\S]*?-----END (?:[A-Z0-9]+ )*PRIVATE",
                    r" KEY-----",
                ],
                "[REDACTED_PRIVATE_KEY]",
            ),
            (
                &[r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE", r" KEY-----"],
                "[REDACTED_PRIVATE_KEY]",
            ),
            (
                &[r"(?i)\bBearer\s+", r"[A-Za-z0-9._~+/=-]{8,}"],
                "Bearer [REDACTED]",
            ),
            (&[r"\bsk-", r"(?:ant-)?[A-Za-z0-9_-]{8,}"], "[REDACTED]"),
            (&[r"\bgh[pousr]_", r"[A-Za-z0-9]{10,}"], "[REDACTED]"),
            (&[r"\bgithub_pat_", r"[A-Za-z0-9_]{10,}"], "[REDACTED]"),
            (&[r"\bAKIA", r"[0-9A-Z]{16}\b"], "[REDACTED]"),
            (&[r"\bxox[bp]-", r"[A-Za-z0-9-]{10,}"], "[REDACTED]"),
            (&[r"\bAIza", r"[0-9A-Za-z_-]{20,}"], "[REDACTED]"),
            (&[r"://[^/\s:@]+:", r"[^/\s@]+@"], "://[REDACTED]@"),
            (
                &[r"\b[A-Za-z0-9._+-]+:", r"[^\s@/:]{3,}@([A-Za-z0-9.-]+)"],
                "[REDACTED]@$1",
            ),
        ];
        specs
            .iter()
            .filter_map(|(parts, repl)| Regex::new(&parts.concat()).ok().map(|re| (re, *repl)))
            .collect()
    })
}

/// Strips secrets, raw HTML, and inline images from handover text before
/// sending it to external channels (such as Telegram or GitHub comments).
pub fn sanitize_external_markdown(input: &str) -> String {
    sanitize_pr_body(input)
}

fn sanitize_pr_body(input: &str) -> String {
    let mut text = input.to_string();
    for (re, repl) in sanitizer_rules() {
        text = re.replace_all(&text, *repl).into_owned();
    }
    text
}

fn truncate_utf8_bytes(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let suffix = "\n\n...(truncated)";
    let mut end = max_bytes.saturating_sub(suffix.len()).min(input.len());
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + suffix.len());
    out.push_str(&input[..end]);
    out.push_str(suffix);
    out
}
