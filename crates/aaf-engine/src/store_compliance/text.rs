//! Small text helpers shared by the platform scanners.

use std::fs;
use std::io::Read;
use std::path::Path;

/// Generous for hand-written config; `project.pbxproj` of a large app is a
/// few megabytes.
pub(crate) const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Read a file with a size bound. The error is a short human reason.
pub(crate) fn read_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".to_string());
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!("larger than {MAX_FILE_BYTES} bytes"));
    }
    let mut buf = Vec::new();
    fs::File::open(path)
        .and_then(|f| f.take(MAX_FILE_BYTES).read_to_end(&mut buf))
        .map_err(|e| e.to_string())?;
    Ok(buf)
}

pub(crate) fn read_text(path: &Path) -> Result<String, String> {
    String::from_utf8(read_bytes(path)?).map_err(|_| "not valid UTF-8".to_string())
}

/// Source code is only searched for tokens, so undecodable bytes may be lossy.
pub(crate) fn read_text_lossy(path: &Path) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&read_bytes(path)?).into_owned())
}

/// Decode a `.strings` file: UTF-16 with a BOM, otherwise UTF-8.
pub(crate) fn decode_strings_file(bytes: &[u8]) -> String {
    let utf16 = |be: bool| {
        let units: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| {
                if be {
                    u16::from_be_bytes(*c)
                } else {
                    u16::from_le_bytes(*c)
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xFF, 0xFE, ..] => utf16(false),
        [0xFE, 0xFF, ..] => utf16(true),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// 1-based line of a byte offset.
pub(crate) fn line_of(text: &str, offset: usize) -> u32 {
    let end = offset.min(text.len());
    let newlines = text.as_bytes()[..end]
        .iter()
        .filter(|b| **b == b'\n')
        .count();
    u32::try_from(newlines + 1).unwrap_or(u32::MAX)
}

/// 1-based line of the first occurrence of `needle`.
pub(crate) fn line_of_needle(text: &str, needle: &str) -> Option<u32> {
    text.find(needle).map(|at| line_of(text, at))
}

pub(crate) fn line_number(index: usize) -> Option<u32> {
    u32::try_from(index + 1).ok()
}

/// Blank out comments (and, unless `keep_strings`, string contents) of a
/// C-family source while keeping every newline, so that line numbers still
/// match and a token inside a comment or a string is never mistaken for code.
pub(crate) fn strip_comments(src: &str, keep_strings: bool) -> String {
    #[derive(PartialEq)]
    enum State {
        Code,
        Line,
        Block,
        Str(char),
        Triple,
    }
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut state = State::Code;
    let mut i = 0;
    let at = |i: usize| chars.get(i).copied().unwrap_or('\0');
    while i < chars.len() {
        let c = chars[i];
        match state {
            State::Code => {
                if c == '/' && at(i + 1) == '/' {
                    state = State::Line;
                    i += 2;
                    continue;
                }
                if c == '/' && at(i + 1) == '*' {
                    state = State::Block;
                    i += 2;
                    continue;
                }
                if c == '"' && at(i + 1) == '"' && at(i + 2) == '"' {
                    state = State::Triple;
                    out.push_str("\"\"\"");
                    i += 3;
                    continue;
                }
                if c == '"' || c == '\'' {
                    state = State::Str(c);
                }
                out.push(c);
            }
            State::Line => {
                if c == '\n' {
                    state = State::Code;
                    out.push('\n');
                }
            }
            State::Block => {
                if c == '*' && at(i + 1) == '/' {
                    state = State::Code;
                    i += 2;
                    continue;
                }
                if c == '\n' {
                    out.push('\n');
                }
            }
            State::Str(quote) => {
                if c == '\\' && at(i + 1) != '\n' && i + 1 < chars.len() {
                    if keep_strings {
                        out.push(c);
                        out.push(at(i + 1));
                    }
                    i += 2;
                    continue;
                }
                // A single-line literal cannot span lines; resynchronise.
                if c == quote || c == '\n' {
                    state = State::Code;
                    out.push(c);
                } else if keep_strings {
                    out.push(c);
                }
            }
            State::Triple => {
                if c == '"' && at(i + 1) == '"' && at(i + 2) == '"' {
                    state = State::Code;
                    out.push_str("\"\"\"");
                    i += 3;
                    continue;
                }
                if c == '\n' || keep_strings {
                    out.push(c);
                }
            }
        }
        i += 1;
    }
    out
}

/// Repo-relative path with forward slashes, for findings.
pub(crate) fn rel_str(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}
