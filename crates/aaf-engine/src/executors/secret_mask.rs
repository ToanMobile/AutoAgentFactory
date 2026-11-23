//! OpenAgentd-inspired secret masking with `[secret NAME]` labels and
//! streaming chunk-boundary safety.
//!
//! Named environment secrets (>= [`MIN_NAMED_SECRET_BYTES`]) are replaced with
//! `[secret <NAME>]` so operators can see *which* secret was redacted without
//! leaking its value. Unnamed runtime secrets and high-confidence credential
//! patterns are replaced with `[REDACTED]`.

use std::sync::OnceLock;

use regex::{Captures, Regex};

/// Minimum byte length for named environment secrets to avoid false-positive
/// redaction of short common words (matching OpenAgentd's design).
pub const MIN_NAMED_SECRET_BYTES: usize = 8;

/// Minimum character count for unnamed runtime secrets.
pub const MIN_UNNAMED_SECRET_BYTES: usize = 4;

/// Maximum tail bytes held back across streaming chunks for exact secret
/// matching (caps memory overhead for huge secrets).
pub const MAX_HOLDBACK_BYTES: usize = 256;

/// Maximum bytes held back when an unclosed PEM private key block is active.
const MAX_PEM_HOLDBACK_BYTES: usize = 4096;

const REDACTED: &str = "[REDACTED]";

/// Substrings in an uppercased environment variable name that classify its
/// value as a secret.
const SECRET_NAME_SUBSTRINGS: &[&str] = &[
    "_TOKEN",
    "_KEY",
    "_SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "PRIVATE_KEY",
    "DATABASE_URL",
    "AUTH_HEADER",
    "BEARER",
    "COOKIE",
];

/// Exact uppercased environment variable names that are always secrets.
const SECRET_EXACT_NAMES: &[&str] = &["TOKEN", "SECRET"];

/// Exact uppercased environment variable names excluded as benign.
const EXCLUDED_EXACT_NAMES: &[&str] = &["SSH_AUTH_SOCK", "COLORTERM", "TERM_SESSION_ID", "KEYMAP"];

/// Substrings in an uppercased environment variable name excluded as benign.
const EXCLUDED_SUBSTRINGS: &[&str] = &["PUBLIC_KEY", "KEYMAP"];

/// A secret value paired with the logical variable name shown in
/// `[secret <NAME>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedSecret {
    pub name: String,
    pub value: String,
}

impl NamedSecret {
    /// Creates a new [`NamedSecret`].
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// Sanitizes a secret name to strictly `[A-Z0-9_]+` for `[secret <NAME>]`.
///
/// Falls back to `"SECRET"` if no ASCII alphanumeric characters remain.
pub fn sanitize_secret_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_underscore = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_uppercase());
            prev_underscore = false;
        } else if !out.is_empty() && !prev_underscore {
            out.push('_');
            prev_underscore = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        "SECRET".to_string()
    } else {
        out
    }
}

/// Collects secret-looking variables from the current process environment.
pub fn collect_env_secrets() -> Vec<NamedSecret> {
    collect_env_secrets_from(std::env::vars())
}

/// Collects secret-looking variables from an arbitrary `(name, value)` iterator.
///
/// Matches names containing (case-insensitively) `_TOKEN`, `_KEY`, `_SECRET`,
/// `PASSWORD`, `PASSWD`, `CREDENTIAL`, `PRIVATE_KEY`, `DATABASE_URL`,
/// `AUTH_HEADER`, `BEARER`, or `COOKIE`, while excluding benign variables such
/// as `SSH_AUTH_SOCK`, `*PUBLIC_KEY*`, `*KEYMAP*`, `COLORTERM`, and
/// `TERM_SESSION_ID`. Values shorter than [`MIN_NAMED_SECRET_BYTES`] are
/// ignored. Results are sorted by `value.len()` descending so longer secrets
/// always match before shorter substrings.
pub fn collect_env_secrets_from<I, K, V>(iter: I) -> Vec<NamedSecret>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut secrets: Vec<NamedSecret> = iter
        .into_iter()
        .filter_map(|(k, v)| {
            let name = k.as_ref();
            let value = v.as_ref();
            if value.len() < MIN_NAMED_SECRET_BYTES || !is_secret_env_name(name) {
                return None;
            }
            Some(NamedSecret {
                name: name.to_string(),
                value: value.to_string(),
            })
        })
        .collect();
    sort_named_secrets(&mut secrets);
    secrets
}

fn is_secret_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if EXCLUDED_EXACT_NAMES.contains(&upper.as_str())
        || EXCLUDED_SUBSTRINGS
            .iter()
            .any(|needle| upper.contains(needle))
    {
        return false;
    }
    SECRET_EXACT_NAMES.contains(&upper.as_str())
        || SECRET_NAME_SUBSTRINGS
            .iter()
            .any(|needle| upper.contains(needle))
}

fn sort_named_secrets(secrets: &mut Vec<NamedSecret>) {
    secrets.sort_by(|a, b| {
        b.value
            .len()
            .cmp(&a.value.len())
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.value.cmp(&b.value))
    });
    secrets.dedup();
}

fn normalize_unnamed(unnamed: &[String]) -> Vec<String> {
    let mut cleaned: Vec<String> = unnamed
        .iter()
        .filter(|s| s.len() >= MIN_UNNAMED_SECRET_BYTES)
        .cloned()
        .collect();
    cleaned.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    cleaned.dedup();
    cleaned
}

/// Replaces named secrets with `[secret <NAME>]`, unnamed secrets (length >= 4)
/// with `[REDACTED]`, and high-confidence credential patterns with
/// `[REDACTED]`.
pub fn mask_named_and_patterns(text: &str, named: &[NamedSecret], unnamed: &[String]) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut named_sorted: Vec<&NamedSecret> = named
        .iter()
        .filter(|s| s.value.len() >= MIN_NAMED_SECRET_BYTES)
        .collect();
    named_sorted.sort_by(|a, b| {
        b.value
            .len()
            .cmp(&a.value.len())
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut unnamed_sorted: Vec<&str> = unnamed
        .iter()
        .map(String::as_str)
        .filter(|s| s.len() >= MIN_UNNAMED_SECRET_BYTES)
        .collect();
    unnamed_sorted.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    unnamed_sorted.dedup();

    let mut out = text.to_string();
    for secret in named_sorted {
        if out.contains(secret.value.as_str()) {
            let label = format!("[secret {}]", sanitize_secret_name(&secret.name));
            out = out.replace(secret.value.as_str(), &label);
        }
    }
    for secret in unnamed_sorted {
        if out.contains(secret) {
            out = out.replace(secret, REDACTED);
        }
    }
    scrub_patterns_only(&out)
}

fn scrub_patterns_only(input: &str) -> String {
    let Some(patterns) = mask_patterns() else {
        return input.to_string();
    };
    let mut out = patterns.pem_block.replace_all(input, REDACTED).into_owned();
    out = patterns.pem_line.replace_all(&out, REDACTED).into_owned();
    out = patterns
        .bearer
        .replace_all(&out, |caps: &Captures<'_>| {
            let whole = caps.get(0).map_or("", |m| m.as_str());
            let token = caps.get(1).map_or("", |m| m.as_str());
            if token.starts_with("[secret") || token == REDACTED {
                whole.to_string()
            } else {
                "Bearer [REDACTED]".to_string()
            }
        })
        .into_owned();
    out = patterns
        .url_creds
        .replace_all(&out, "://[REDACTED]@")
        .into_owned();
    for key_re in &patterns.api_keys {
        out = key_re.replace_all(&out, REDACTED).into_owned();
    }
    out
}

/// Stateful streaming secret masker that buffers enough tail bytes to catch
/// secrets and credential patterns split across chunk boundaries.
#[derive(Debug, Clone)]
pub struct ChunkMasker {
    named: Vec<NamedSecret>,
    unnamed: Vec<String>,
    pending: String,
    holdback_bytes: usize,
}

impl ChunkMasker {
    /// Creates a new [`ChunkMasker`] with sorted named and unnamed secrets.
    ///
    /// `holdback_bytes` is computed as `max_secret_len - 1`, capped at
    /// [`MAX_HOLDBACK_BYTES`].
    pub fn new(named: Vec<NamedSecret>, unnamed: Vec<String>) -> Self {
        let mut named: Vec<NamedSecret> = named
            .into_iter()
            .filter(|s| s.value.len() >= MIN_NAMED_SECRET_BYTES)
            .collect();
        sort_named_secrets(&mut named);
        let unnamed = normalize_unnamed(&unnamed);

        let max_named = named.first().map_or(0, |s| s.value.len());
        let max_unnamed = unnamed.first().map_or(0, String::len);
        let max_len = max_named.max(max_unnamed);
        let holdback_bytes = max_len.saturating_sub(1).min(MAX_HOLDBACK_BYTES);

        Self {
            named,
            unnamed,
            pending: String::new(),
            holdback_bytes,
        }
    }

    /// Returns the exact-secret holdback window in bytes.
    pub fn holdback_bytes(&self) -> usize {
        self.holdback_bytes
    }

    /// Appends `chunk` to the internal buffer and returns the masked prefix
    /// that can be emitted safely without splitting any secret or UTF-8 code
    /// point across chunk boundaries.
    pub fn push(&mut self, chunk: &str) -> String {
        if chunk.is_empty() {
            return String::new();
        }
        self.pending.push_str(chunk);

        let cut = self.safe_emit_cut();
        if cut == 0 {
            return String::new();
        }
        let remainder = self.pending.split_off(cut);
        let prefix = std::mem::replace(&mut self.pending, remainder);
        mask_named_and_patterns(&prefix, &self.named, &self.unnamed)
    }

    /// Flushes and masks all remaining buffered bytes.
    pub fn finish(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let remaining = std::mem::take(&mut self.pending);
        mask_named_and_patterns(&remaining, &self.named, &self.unnamed)
    }

    fn safe_emit_cut(&self) -> usize {
        let len = self.pending.len();
        if len <= self.holdback_bytes {
            return 0;
        }
        let mut cut = floor_char_boundary(&self.pending, len - self.holdback_bytes);
        if cut == 0 {
            return 0;
        }

        // Hold back an unclosed PEM header if within the PEM window so a
        // multi-chunk private key block is scrubbed as a single unit.
        if let Some(pem_start) = unclosed_pem_start(&self.pending)
            && len.saturating_sub(pem_start) <= MAX_PEM_HOLDBACK_BYTES
        {
            cut = cut.min(pem_start);
        }

        // Keep trailing non-whitespace token intact (up to MAX_HOLDBACK_BYTES)
        // so pattern-based tokens (sk-ant-..., Telegram, AKIA..., URL creds)
        // split across chunks are never emitted halfway.
        cut = back_up_trailing_token(&self.pending, cut);

        // If the prefix ends with `Bearer ` (or `Bearer\t`), hold it back so a
        // Bearer token arriving in the next chunk is caught.
        cut = back_up_trailing_bearer(&self.pending, cut);

        // Never split inside any exact secret match that straddles `cut`.
        cut = self.back_up_straddling_secrets(cut);

        floor_char_boundary(&self.pending, cut)
    }

    fn back_up_straddling_secrets(&self, mut cut: usize) -> usize {
        loop {
            if cut == 0 {
                return 0;
            }
            let mut backed_up = cut;
            for secret in self
                .named
                .iter()
                .map(|s| s.value.as_str())
                .chain(self.unnamed.iter().map(String::as_str))
            {
                let slen = secret.len();
                if slen < 2 {
                    continue;
                }
                let scan_start = floor_char_boundary(&self.pending, cut.saturating_sub(slen));
                let mut pos = scan_start;
                while let Some(rel) = self.pending[pos..].find(secret) {
                    let m_start = pos + rel;
                    let m_end = m_start + slen;
                    if m_start >= cut {
                        break;
                    }
                    if m_end > cut {
                        backed_up = backed_up.min(m_start);
                        break;
                    }
                    pos = m_end;
                }
            }
            if backed_up == cut {
                return cut;
            }
            cut = backed_up;
        }
    }
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn unclosed_pem_start(text: &str) -> Option<usize> {
    let begin_marker = ["-----BEGIN ", "PRIVATE KEY-----"].concat();
    let begin_generic = "-----BEGIN ";
    let end_marker = "-----END ";
    let last_begin = text.rfind(begin_generic)?;
    let tail = &text[last_begin..];
    if tail.contains(end_marker) {
        return None;
    }
    if tail.contains("PRIVATE") || tail.contains(&begin_marker) || tail.len() < 48 {
        Some(last_begin)
    } else {
        None
    }
}

fn back_up_trailing_token(text: &str, cut: usize) -> usize {
    if cut == 0 || cut > text.len() {
        return cut;
    }
    let bytes = text.as_bytes();
    let left_is_ws = bytes[cut - 1].is_ascii_whitespace();
    if left_is_ws {
        return cut;
    }
    let right_is_ws = cut < text.len() && bytes[cut].is_ascii_whitespace();
    if right_is_ws {
        return cut;
    }
    // `cut` is either strictly inside a non-whitespace token or at the very end
    // of a still-open trailing token; back up to its start if within cap.
    let token_start = text[..cut]
        .rfind(|c: char| c.is_ascii_whitespace())
        .map_or(0, |ws| ws + 1);
    if text.len().saturating_sub(token_start) <= MAX_HOLDBACK_BYTES {
        token_start
    } else {
        cut
    }
}

fn back_up_trailing_bearer(text: &str, cut: usize) -> usize {
    let prefix = &text[..cut];
    let trimmed = prefix.trim_end_matches([' ', '\t']);
    if trimmed.len() == prefix.len() {
        return cut;
    }
    const BEARER_LEN: usize = 6;
    if trimmed.len() >= BEARER_LEN {
        let start = trimmed.len() - BEARER_LEN;
        if trimmed.is_char_boundary(start)
            && trimmed[start..].eq_ignore_ascii_case("bearer")
            && (start == 0
                || !trimmed[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_'))
        {
            return start;
        }
    }
    cut
}

struct MaskPatterns {
    pem_block: Regex,
    pem_line: Regex,
    bearer: Regex,
    url_creds: Regex,
    api_keys: Vec<Regex>,
}

fn compile_parts(parts: &[&str]) -> Option<Regex> {
    Regex::new(&parts.concat()).ok()
}

/// Patterns are assembled from fragments so static secret scanners never
/// false-positive on this source file.
fn mask_patterns() -> Option<&'static MaskPatterns> {
    static PATTERNS: OnceLock<Option<MaskPatterns>> = OnceLock::new();
    PATTERNS
        .get_or_init(|| {
            let pem_block = compile_parts(&[
                "(?s)-----BEGIN (?:[A-Z0-9]+ )*PRIVATE",
                " KEY-----.*?-----END (?:[A-Z0-9]+ )*PRIVATE",
                " KEY-----",
            ])?;
            let pem_line =
                compile_parts(&["-----BEGIN (?:[A-Z0-9]+ )*PRIVATE", " KEY-----[^\\r\\n]*"])?;
            let bearer = Regex::new(r#"(?i)\bBearer[ \t]+([^\s"',;]+)"#).ok()?;
            let url_creds = Regex::new(r#"://[^\s/@:"']+:[^\s/@"']+@"#).ok()?;
            let api_key_specs: &[&[&str]] = &[
                &[r"\bsk-", "ant-[A-Za-z0-9_-]{4,}"],
                &[r"\bsk-", "[A-Za-z0-9_-]{4,}"],
                &[r"\bgh[pousr]_", "[A-Za-z0-9_]{4,}"],
                &[r"\bgithub_", "pat_[A-Za-z0-9_]{4,}"],
                &[r"\bAK", "IA[0-9A-Z]{6,}"],
                &[r"\bAI", "za[0-9A-Za-z_-]{10,}"],
                &[r"\bxox[baprs]-", "[A-Za-z0-9-]{6,}"],
                &[r"\b\d{8,10}:", "[A-Za-z0-9_-]{35}\\b"],
            ];
            let mut api_keys = Vec::with_capacity(api_key_specs.len());
            for spec in api_key_specs {
                api_keys.push(compile_parts(spec)?);
            }
            Some(MaskPatterns {
                pem_block,
                pem_line,
                bearer,
                url_creds,
                api_keys,
            })
        })
        .as_ref()
}
