//! Payload field extractors and extended host adapters (`antigravity`, `opencode`)
//! for [`super`].

use serde_json::{Map, Value};

use super::{ToolAction, bad_input};
use crate::Denial;

/// Extracts every file path referenced by a Codex / OpenCode `apply_patch` document.
///
/// Recognizes `*** Update File:`, `*** Delete File:`, `*** Add File:`, and
/// `*** Move to:` headers, preserving first-seen order without duplicates.
pub fn extract_apply_patch_paths(patch: &str) -> Vec<String> {
    const PREFIXES: &[&str] = &[
        "*** Update File:",
        "*** Delete File:",
        "*** Add File:",
        "*** Move to:",
    ];
    let mut paths = Vec::new();
    for raw_line in patch.lines() {
        let line = raw_line.trim_end_matches('\r');
        for prefix in PREFIXES {
            if let Some(rest) = line.strip_prefix(prefix) {
                let candidate = rest.trim();
                if !candidate.is_empty() && !paths.iter().any(|existing| existing == candidate) {
                    paths.push(candidate.to_string());
                }
                break;
            }
        }
    }
    paths
}

fn shell_quote_arg(arg: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !arg.is_empty() && arg.chars().all(plain) {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

pub(super) fn extract_shell_command(
    tool_input: Option<&Value>,
    allow_cmd_alias: bool,
    allow_array: bool,
) -> Result<String, Denial> {
    let Some(obj) = tool_input.and_then(Value::as_object) else {
        return Err(bad_input("shell tool call without a tool_input object"));
    };
    let field = obj.get("command").or_else(|| {
        if allow_cmd_alias {
            obj.get("cmd").or_else(|| obj.get("CommandLine"))
        } else {
            None
        }
    });
    let Some(value) = field else {
        return Err(bad_input(if allow_cmd_alias {
            "shell tool call without tool_input.command or tool_input.cmd"
        } else {
            "Bash call without tool_input.command"
        }));
    };
    match value {
        Value::String(cmd) => Ok(cmd.clone()),
        Value::Array(items) if allow_array => {
            if items.is_empty() {
                return Err(bad_input("shell tool call with empty command array"));
            }
            let mut quoted = Vec::with_capacity(items.len());
            for item in items {
                let Some(part) = item.as_str() else {
                    return Err(bad_input(
                        "shell tool call command array contains non-string element",
                    ));
                };
                quoted.push(shell_quote_arg(part));
            }
            Ok(quoted.join(" "))
        }
        _ => Err(bad_input("shell tool call command has unsupported type")),
    }
}

pub(super) fn extract_file_path(
    tool_input: Option<&Value>,
    allow_path_alias: bool,
) -> Result<String, Denial> {
    let Some(obj) = tool_input.and_then(Value::as_object) else {
        return Err(bad_input("file tool call without a tool_input object"));
    };
    let value = obj.get("file_path").and_then(Value::as_str).or_else(|| {
        if allow_path_alias {
            obj.get("path")
                .or_else(|| obj.get("filePath"))
                .or_else(|| obj.get("TargetFile"))
                .or_else(|| obj.get("target_file"))
                .or_else(|| obj.get("AbsolutePath"))
                .or_else(|| obj.get("Path"))
                .and_then(Value::as_str)
        } else {
            None
        }
    });
    match value {
        Some(path) if !path.is_empty() => Ok(path.to_string()),
        _ => Err(bad_input(if allow_path_alias {
            "file tool call without tool_input.file_path or tool_input.path"
        } else {
            "file tool call without tool_input.file_path"
        })),
    }
}

pub(super) fn extract_many_paths(tool_input: Option<&Value>) -> Result<Vec<String>, Denial> {
    let Some(obj) = tool_input.and_then(Value::as_object) else {
        return Err(bad_input(
            "read_many_files call without a tool_input object",
        ));
    };
    let Some(paths_val) = obj.get("paths") else {
        return Err(bad_input("read_many_files call without tool_input.paths"));
    };
    let Some(items) = paths_val.as_array() else {
        return Err(bad_input(
            "read_many_files tool_input.paths is not an array",
        ));
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(path) = item.as_str() else {
            return Err(bad_input(
                "read_many_files tool_input.paths contains a non-string entry",
            ));
        };
        if !path.is_empty() {
            out.push(path.to_string());
        }
    }
    if let Some(include_val) = obj.get("include") {
        let Some(include_items) = include_val.as_array() else {
            return Err(bad_input(
                "read_many_files tool_input.include is not an array",
            ));
        };
        for item in include_items {
            let Some(path) = item.as_str() else {
                return Err(bad_input(
                    "read_many_files tool_input.include contains a non-string entry",
                ));
            };
            if !path.is_empty() {
                out.push(path.to_string());
            }
        }
    }
    Ok(out)
}

pub(super) fn extract_apply_patch(tool_input: Option<&Value>) -> Result<Vec<String>, Denial> {
    let patch_text = match tool_input {
        Some(Value::String(raw)) => Some(raw.as_str()),
        Some(Value::Object(obj)) => obj
            .get("patch")
            .and_then(Value::as_str)
            .or_else(|| obj.get("input").and_then(Value::as_str))
            .or_else(|| obj.get("diff").and_then(Value::as_str)),
        _ => None,
    };
    let Some(patch) = patch_text.filter(|text| !text.trim().is_empty()) else {
        return Err(bad_input(
            "apply_patch call without tool_input.patch or tool_input.input",
        ));
    };
    let paths = extract_apply_patch_paths(patch);
    if paths.is_empty() {
        return Err(bad_input(
            "apply_patch call contains no recognized target file paths",
        ));
    }
    Ok(paths)
}

fn extract_opencode_patch(tool_input: Option<&Value>) -> Result<Vec<String>, Denial> {
    if let Ok(paths) = extract_apply_patch(tool_input) {
        return Ok(paths);
    }
    extract_file_path(tool_input, true).map(|path| vec![path])
}

pub(super) fn extract_cwd(payload: &Map<String, Value>) -> Option<String> {
    payload
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .or_else(|| {
            payload
                .get("workspace_root")
                .and_then(Value::as_str)
                .filter(|cwd| !cwd.is_empty())
        })
        .map(str::to_string)
}

pub(super) fn extract_cwd_override(tool_input: Option<&Value>) -> Result<Option<String>, Denial> {
    let Some(obj) = tool_input.and_then(Value::as_object) else {
        return Ok(None);
    };
    let mut chosen = None;
    for field in ["workdir", "directory", "dir_path", "Cwd"] {
        if let Some(val) = obj.get(field) {
            let Some(dir) = val.as_str() else {
                return Err(bad_input(format!(
                    "tool_input.{field} must be a JSON string"
                )));
            };
            if chosen.is_none() && !dir.is_empty() {
                chosen = Some(dir.to_string());
            }
        }
    }
    Ok(chosen)
}

pub(super) fn parse_antigravity_action(
    tool: &str,
    tool_input: Option<&Value>,
) -> Result<ToolAction, Denial> {
    match tool {
        "run_command" | "run_shell_command" | "Bash" | "shell" => {
            extract_shell_command(tool_input, true, true).map(ToolAction::Shell)
        }
        "write_to_file"
        | "replace_file_content"
        | "multi_replace_file_content"
        | "edit_file"
        | "write_file"
        | "replace"
        | "Write"
        | "Edit"
        | "MultiEdit" => {
            extract_file_path(tool_input, true).map(|path| ToolAction::FileWrite(vec![path]))
        }
        "view_file" | "read_file" | "Read" => {
            extract_file_path(tool_input, true).map(ToolAction::FileRead)
        }
        "read_many_files" => extract_many_paths(tool_input).map(ToolAction::FileReadMany),
        _ => Ok(ToolAction::Unknown),
    }
}

pub(super) fn parse_opencode_action(
    tool: &str,
    tool_input: Option<&Value>,
) -> Result<ToolAction, Denial> {
    match tool {
        "bash" | "Bash" | "shell" | "run_shell_command" => {
            extract_shell_command(tool_input, true, true).map(ToolAction::Shell)
        }
        "write" | "edit" | "write_file" | "edit_file" | "Write" | "Edit" | "MultiEdit" => {
            extract_file_path(tool_input, true).map(|path| ToolAction::FileWrite(vec![path]))
        }
        "patch" | "apply_patch" => extract_opencode_patch(tool_input).map(ToolAction::FileWrite),
        "read" | "Read" | "read_file" | "view_file" => {
            extract_file_path(tool_input, true).map(ToolAction::FileRead)
        }
        _ => Ok(ToolAction::Unknown),
    }
}
