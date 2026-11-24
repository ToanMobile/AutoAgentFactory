//! Safe, bounded JSONL stream log for live cockpit inspection.
//!
//! Writes only compact, secret-scrubbed projection lines to
//! `<data_dir>/streams/<run_id>.jsonl` with `0700`/`0600` permissions and a
//! hard [`MAX_STREAM_LOG_BYTES`] cap per run. Raw `stream-json` is never
//! written to disk.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use super::reaper::validate_run_id;
use super::secret_mask::{self, NamedSecret};

/// Maximum bytes written to a single run's stream log (`2 MiB`).
pub const MAX_STREAM_LOG_BYTES: usize = 2 * 1024 * 1024;
/// Maximum UTF-8 byte length of the `target` (or `tool`) summary field.
pub const MAX_TARGET_BYTES: usize = 512;

const STREAMS_DIR: &str = "streams";
const TRUNCATED_LINE: &str = "{\"type\":\"truncated\"}\n";
const MIN_ENV_SECRET_CHARS: usize = 4;
const READ_TAIL_EXTRA_BYTES: usize = 4096;
const INITIAL_TAIL_WINDOW: u64 = 32 * 1024;

#[derive(Serialize)]
struct LogEntry<'a> {
    ts: u64,
    #[serde(rename = "type")]
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<&'a str>,
}

/// Append-only, secret-scrubbed stream projection writer for one agent run.
#[derive(Debug)]
pub struct StreamLog {
    file: Option<File>,
    bytes_written: usize,
    truncated: bool,
    named_secrets: Vec<NamedSecret>,
    extra_secrets: Vec<String>,
}

impl StreamLog {
    /// A no-op writer used when `data_dir` is absent or best-effort opening fails.
    pub fn noop() -> Self {
        Self {
            file: None,
            bytes_written: 0,
            truncated: false,
            named_secrets: Vec::new(),
            extra_secrets: Vec::new(),
        }
    }

    /// Opens `<data_dir>/streams/<run_id>.jsonl` for exclusive creation (`0600`).
    ///
    /// Returns a no-op writer when `data_dir` is `None`. Fails closed on
    /// invalid `run_id`, symlinks, or pre-existing files.
    pub fn open(
        data_dir: Option<&Path>,
        run_id: &str,
        extra_secrets: Vec<String>,
    ) -> io::Result<Self> {
        let Some(data_dir) = data_dir else {
            return Ok(Self::noop());
        };
        let path = validated_stream_path(data_dir, run_id)?;
        let streams_dir = data_dir.join(STREAMS_DIR);
        ensure_private_dir(&streams_dir)?;

        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let file = opts.open(&path)?;
        #[cfg(unix)]
        file.set_permissions(fs::Permissions::from_mode(0o600))?;

        Ok(Self {
            file: Some(file),
            bytes_written: 0,
            truncated: false,
            named_secrets: secret_mask::collect_env_secrets(),
            extra_secrets: normalize_secrets(extra_secrets),
        })
    }

    /// Total bytes written to disk so far (including the truncation marker if set).
    pub fn bytes_written(&self) -> usize {
        self.bytes_written
    }

    /// True once the [`MAX_STREAM_LOG_BYTES`] cap has been hit.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Writes a single compact, secret-scrubbed JSON projection line.
    pub fn write_event(&mut self, kind: &str, tool: Option<&str>, target: Option<&str>) {
        if self.truncated || self.file.is_none() {
            return;
        }
        let clean_tool = tool.map(|t| sanitize_field(t, &self.named_secrets, &self.extra_secrets));
        let clean_target =
            target.map(|t| sanitize_field(t, &self.named_secrets, &self.extra_secrets));
        let entry = LogEntry {
            ts: unix_secs(),
            kind,
            tool: clean_tool.as_deref(),
            target: clean_target.as_deref(),
        };
        let Ok(mut line) = serde_json::to_string(&entry) else {
            return;
        };
        line.push('\n');
        self.write_bounded_line(&line);
    }

    /// Emits a `"start"` projection line for an executor run.
    pub fn write_start(&mut self, executor: &str, detail: Option<&str>) {
        let detail = detail.map(str::trim).filter(|s| !s.is_empty());
        self.write_event("start", Some(executor), detail);
    }

    /// Emits a `"finish"` projection line for a Claude Code run.
    pub fn write_claude_finish(
        &mut self,
        exit_code: Option<i32>,
        timed_out: bool,
        tokens_in: u64,
        tokens_out: u64,
    ) {
        let exit = format_exit(exit_code);
        let target = format!(
            "exit={exit} timed_out={timed_out} tokens_in={tokens_in} tokens_out={tokens_out}"
        );
        self.write_event("finish", Some("claude"), Some(&target));
    }

    /// Emits a `"finish"` projection line for a generic CLI run with a
    /// scrubbed tail excerpt.
    pub fn write_generic_finish(
        &mut self,
        program: &str,
        exit_code: Option<i32>,
        timed_out: bool,
        raw_tail: &str,
    ) {
        let exit = format_exit(exit_code);
        let scrubbed_tail = secret_mask::mask_named_and_patterns(
            raw_tail,
            &self.named_secrets,
            &self.extra_secrets,
        );
        let tail_line = scrubbed_tail
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty());
        let target = match tail_line {
            Some(line) => format!("exit={exit} timed_out={timed_out} {line}"),
            None => format!("exit={exit} timed_out={timed_out}"),
        };
        self.write_event("finish", Some(program), Some(&target));
    }

    /// Projects one line of Claude `stream-json` into compact sanitized events.
    ///
    /// Never writes raw stream JSON; ignores `system`, `user`, and unparseable lines.
    pub fn record_claude_line(&mut self, line: &str) {
        if self.truncated || self.file.is_none() {
            return;
        }
        let Ok(Value::Object(event)) = serde_json::from_str::<Value>(line.trim()) else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let Some(blocks) = event
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                else {
                    return;
                };
                let mut text_parts: Vec<&str> = Vec::new();
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("tool_use") => {
                            let name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown");
                            let target = block.get("input").and_then(extract_tool_target);
                            self.write_event("tool_use", Some(name), target.as_deref());
                        }
                        Some("text") => {
                            if let Some(t) = block.get("text").and_then(Value::as_str) {
                                let trimmed = t.trim();
                                if !trimmed.is_empty() {
                                    text_parts.push(trimmed);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if !text_parts.is_empty() {
                    let joined = text_parts.join(" ");
                    self.write_event("assistant", None, Some(&joined));
                }
            }
            Some("result") => {
                let is_error = event
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let subtype = event
                    .get("subtype")
                    .and_then(Value::as_str)
                    .unwrap_or(if is_error { "error" } else { "ok" });
                let text = event
                    .get("result")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("");
                let summary = if text.is_empty() {
                    subtype.to_string()
                } else {
                    format!("{subtype}: {text}")
                };
                self.write_event("result", None, Some(&summary));
            }
            Some("error") => {
                let msg = event
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        event.get("error").and_then(|e| {
                            e.as_str()
                                .or_else(|| e.get("message").and_then(Value::as_str))
                        })
                    })
                    .unwrap_or("error");
                self.write_event("error", None, Some(msg));
            }
            Some("rate_limit_event") => {
                let status = event
                    .get("rate_limit_info")
                    .and_then(|i| i.get("status"))
                    .and_then(Value::as_str);
                if status == Some("rejected") {
                    self.write_event("rate_limit", None, Some("rejected"));
                }
            }
            _ => {}
        }
    }

    fn write_bounded_line(&mut self, line: &str) {
        if self.truncated {
            return;
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let budget = MAX_STREAM_LOG_BYTES.saturating_sub(TRUNCATED_LINE.len());
        if self.bytes_written.saturating_add(line.len()) > budget {
            if file.write_all(TRUNCATED_LINE.as_bytes()).is_ok() {
                self.bytes_written = self.bytes_written.saturating_add(TRUNCATED_LINE.len());
            } else {
                self.file = None;
            }
            self.truncated = true;
            return;
        }
        if file.write_all(line.as_bytes()).is_ok() {
            self.bytes_written = self.bytes_written.saturating_add(line.len());
        } else {
            self.file = None;
        }
    }
}

/// Reads the last `max_lines` non-empty lines from `<data_dir>/streams/<run_id>.jsonl`.
///
/// Validates `run_id`, refuses symlinks on the streams directory and file, and
/// bounds the read to `MAX_STREAM_LOG_BYTES + 4096`.
pub fn read_tail_lines(data_dir: &Path, run_id: &str, max_lines: usize) -> io::Result<Vec<String>> {
    let path = validated_stream_path(data_dir, run_id)?;
    let streams_dir = data_dir.join(STREAMS_DIR);
    let dir_meta = fs::symlink_metadata(&streams_dir)?;
    if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "streams directory is a symlink or not a directory",
        ));
    }
    let meta = fs::symlink_metadata(&path)?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "stream log path is a symlink or not a regular file",
        ));
    }
    let mut file = File::open(&path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "stream log is not a regular file",
        ));
    }
    if max_lines == 0 {
        return Ok(Vec::new());
    }
    let file_len = file.seek(SeekFrom::End(0))?;
    if file_len == 0 {
        return Ok(Vec::new());
    }
    let limit = (MAX_STREAM_LOG_BYTES + READ_TAIL_EXTRA_BYTES) as u64;
    let max_window = file_len.min(limit);
    let mut window = INITIAL_TAIL_WINDOW.min(max_window);
    let mut buf = Vec::new();
    loop {
        let offset = file_len.saturating_sub(window);
        file.seek(SeekFrom::Start(offset))?;
        buf.clear();
        std::io::Read::by_ref(&mut file)
            .take(window)
            .read_to_end(&mut buf)?;
        let slice = if offset > 0 {
            buf.iter()
                .position(|&b| b == b'\n')
                .map_or(&[][..], |pos| &buf[pos + 1..])
        } else {
            &buf[..]
        };
        let text = String::from_utf8_lossy(slice);
        let mut ring: VecDeque<String> = VecDeque::with_capacity(max_lines.min(1024));
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if ring.len() == max_lines {
                ring.pop_front();
            }
            ring.push_back(trimmed.to_string());
        }
        if ring.len() >= max_lines || window >= max_window {
            return Ok(ring.into_iter().collect());
        }
        window = window.saturating_mul(2).min(max_window);
    }
}

/// Truncates `text` to at most `max_bytes` on a valid UTF-8 character boundary.
pub fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Scrubs PEM private keys, Bearer headers, URL credentials, common API key
/// prefixes, named process environment secrets (`[secret NAME]`), and exact
/// runtime values of passed environment secrets.
pub fn scrub_secrets(input: &str, extra_secrets: &[String]) -> String {
    let named = secret_mask::collect_env_secrets();
    secret_mask::mask_named_and_patterns(input, &named, extra_secrets)
}

fn sanitize_field(raw: &str, named_secrets: &[NamedSecret], extra_secrets: &[String]) -> String {
    let scrubbed = secret_mask::mask_named_and_patterns(raw, named_secrets, extra_secrets);
    truncate_utf8(&scrubbed, MAX_TARGET_BYTES).to_string()
}

fn validated_stream_path(data_dir: &Path, run_id: &str) -> io::Result<PathBuf> {
    validate_run_id(run_id)
        .map_err(|err| io::Error::new(ErrorKind::InvalidInput, err.to_string()))?;
    Ok(data_dir.join(STREAMS_DIR).join(format!("{run_id}.jsonl")))
}

fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "streams directory must not be a symlink",
                ));
            }
            if !meta.is_dir() {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "streams path exists and is not a directory",
                ));
            }
            #[cfg(unix)]
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            builder.mode(0o700);
            builder.create(dir)?;
            #[cfg(unix)]
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            Ok(())
        }
        Err(err) => Err(err),
    }
}

fn normalize_secrets(extra_secrets: Vec<String>) -> Vec<String> {
    let mut cleaned: Vec<String> = extra_secrets
        .into_iter()
        .filter(|s| s.chars().count() >= MIN_ENV_SECRET_CHARS)
        .collect();
    cleaned.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    cleaned.dedup();
    cleaned
}

fn extract_tool_target(input: &Value) -> Option<String> {
    const TARGET_KEYS: [&str; 9] = [
        "file_path",
        "path",
        "command",
        "pattern",
        "url",
        "query",
        "description",
        "notebook_path",
        "glob",
    ];
    let obj = input.as_object()?;
    for key in TARGET_KEYS {
        if let Some(val) = obj.get(key).and_then(Value::as_str) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn format_exit(exit_code: Option<i32>) -> String {
    match exit_code {
        Some(code) => code.to_string(),
        None => "none".to_string(),
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn read_tail_lines_seeks_backwards_over_large_file_and_discards_partial_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let streams = dir.path().join(STREAMS_DIR);
        fs::create_dir_all(&streams).unwrap();
        let path = streams.join("run_large.jsonl");

        // ~100 KiB file (> 64 KiB) with multi-byte UTF-8 ('€') so backward seek
        // lands mid-line and potentially mid-character.
        let mut content = String::new();
        for idx in 0..1_300usize {
            content.push_str(&format!(
                "{{\"idx\":{idx},\"msg\":\"€€€-payload-padding-0123456789abcdef-{idx:04}\"}}\n"
            ));
        }
        assert!(content.len() > 80 * 1024);
        fs::write(&path, &content).unwrap();

        // Small tail satisfied within initial 32 KiB window.
        let tail5 = read_tail_lines(dir.path(), "run_large", 5).unwrap();
        assert_eq!(tail5.len(), 5);
        for (offset, line) in tail5.iter().enumerate() {
            assert!(!line.contains('\u{FFFD}'), "corrupted UTF-8: {line}");
            let v: Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["idx"], 1_295 + offset);
        }

        // Larger tail forces window growth (32 KiB -> 64 KiB -> 128 KiB).
        let tail700 = read_tail_lines(dir.path(), "run_large", 700).unwrap();
        assert_eq!(tail700.len(), 700);
        assert_eq!(
            serde_json::from_str::<Value>(&tail700[0]).unwrap()["idx"],
            600
        );
        assert_eq!(
            serde_json::from_str::<Value>(&tail700[699]).unwrap()["idx"],
            1_299
        );

        // Requesting more than total lines reaches offset 0 and keeps line 0 intact.
        let all = read_tail_lines(dir.path(), "run_large", 5_000).unwrap();
        assert_eq!(all.len(), 1_300);
        assert_eq!(serde_json::from_str::<Value>(&all[0]).unwrap()["idx"], 0);
    }

    #[test]
    fn read_tail_lines_handles_empty_single_long_line_and_over_cap_files() {
        let dir = tempfile::tempdir().unwrap();
        let streams = dir.path().join(STREAMS_DIR);
        fs::create_dir_all(&streams).unwrap();

        fs::write(streams.join("run_empty.jsonl"), "").unwrap();
        assert!(
            read_tail_lines(dir.path(), "run_empty", 10)
                .unwrap()
                .is_empty()
        );

        // Single 40 KiB line with no newline forces growth to offset 0.
        let single = "a".repeat(40 * 1024);
        fs::write(streams.join("run_single.jsonl"), &single).unwrap();
        let got = read_tail_lines(dir.path(), "run_single", 5).unwrap();
        assert_eq!(got, vec![single]);

        // File exceeding MAX_STREAM_LOG_BYTES + READ_TAIL_EXTRA_BYTES still tails cleanly.
        let mut huge = "x".repeat(MAX_STREAM_LOG_BYTES + READ_TAIL_EXTRA_BYTES + 1024);
        huge.push_str("\n{\"idx\":1}\n{\"idx\":2}\n{\"idx\":3}\n");
        fs::write(streams.join("run_huge.jsonl"), &huge).unwrap();
        let tail = read_tail_lines(dir.path(), "run_huge", 3).unwrap();
        assert_eq!(tail, vec!["{\"idx\":1}", "{\"idx\":2}", "{\"idx\":3}"]);
    }
}
