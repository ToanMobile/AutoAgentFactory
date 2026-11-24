//! Judgement for text the splitter gave up on (unbalanced quotes, nesting
//! past the limit). No argv is available, so a guarded tool next to a
//! dangerous word is enough to refuse.

use std::path::Path;

use crate::shell::basename;
use crate::{
    Ctx, Denial, data_guard, delete_guard, git_files, git_guard, git_persist, heavy_guard,
    human_guard, interp_guard, paths, store_guard,
};

const RULE: &str = "unparsable_command";
const FIX: &str = "rewrite it as plain commands: balanced quotes, no deep nesting";

pub fn check(text: &str, ctx: &Ctx) -> Option<Denial> {
    // A continued line is one command.
    let joined = text.replace("\\\n", " ");
    joined
        .lines()
        .find_map(|line| line_reason(line, ctx))
        .map(|reason| Denial::new(RULE, format!("cannot analyse a command that {reason}"), FIX))
}

fn line_reason(line: &str, ctx: &Ctx) -> Option<&'static str> {
    let tokens: Vec<&str> = line
        .split(|c: char| c.is_whitespace() || ";|&()<>\"'`{}\\".contains(c))
        .filter(|t| !t.is_empty())
        .collect();
    let has = |name: &str| tokens.iter().any(|t| basename(t) == name);
    let cluster_has = |flag: char| {
        tokens
            .iter()
            .any(|t| t.len() > 1 && t.starts_with('-') && !t.starts_with("--") && t.contains(flag))
    };

    if has("git") {
        let dangerous = tokens.iter().any(|t| {
            git_guard::abbrev(t, "--force", 3)
                || t.starts_with("--force")
                || git_guard::abbrev(t, "--no-verify", 6)
                || git_guard::abbrev(t, "--hard", 3)
                || git_guard::abbrev(t, "--delete", 3)
                || git_guard::abbrev(t, "--mirror", 3)
                || matches!(*t, "-f" | "-d" | "-D")
                || t.to_ascii_lowercase().contains("hookspath")
                || git_guard::reads_unseen_config(t)
                || (t.len() > 1 && t.starts_with(['+', ':']))
        });
        if dangerous
            || cluster_has('f')
            || cluster_has('d')
            || cluster_has('D')
            || (has("commit") && cluster_has('n'))
            || (has("clean") && (cluster_has('x') || cluster_has('X')))
        {
            return Some("mixes git with a history- or hook-changing option");
        }
        let persists = tokens.iter().any(|t| {
            matches!(
                *t,
                "-B" | "-C" | "-M" | "-m" | "update-ref" | "symbolic-ref"
            ) || t.to_ascii_lowercase().contains("template")
                || git_persist::is_ext(t)
                || (has("config") && git_persist::runs_program(t))
        });
        if persists
            || cluster_has('B')
            || cluster_has('C')
            || cluster_has('M')
            || (has("branch") && cluster_has('m'))
        {
            return Some("mixes git with a ref, template or program setting");
        }
    }
    let lower = line.to_ascii_lowercase();
    if [".git/hooks", ".git/config", ".gitconfig", ".git/refs"]
        .iter()
        .any(|file| lower.contains(file))
        || git_files::is_aaf_ref_text(line)
    {
        return Some("names a git control file");
    }
    if line.contains("AAF_RUN_ID=")
        || (line.contains("AAF_RUN_ID") && (has("unset") || has("env") || has("export")))
    {
        return Some("changes AAF_RUN_ID");
    }
    if has("adb") || has("fastboot") {
        let dangerous = tokens.iter().any(|t| {
            let t = t.to_ascii_lowercase();
            matches!(t.as_str(), "reboot" | "erase" | "rm" | "dd" | "format")
                || t.starts_with("flash")
                || t.starts_with("unlock")
                || t.starts_with("mkfs")
                || t.contains("wipe")
        });
        if dangerous {
            return Some("mixes adb/fastboot with a destructive word");
        }
    }
    if (line.contains('>')
        || [
            "rm", "unlink", "truncate", "mv", "shred", "tee", "cp", "install", "rsync", "dd",
        ]
        .iter()
        .any(|tool| has(tool)))
        && tokens.iter().any(|t| {
            paths::protected_name(Path::new(t)).is_some()
                || t.split_once('=')
                    .is_some_and(|(_, val)| paths::protected_name(Path::new(val)).is_some())
        })
    {
        return Some("mixes a delete/move with a protected file name");
    }
    if let Some(rm_idx) = tokens.iter().position(|t| basename(t) == "rm")
        && (tokens.contains(&"--recursive") || cluster_has('r') || cluster_has('R'))
        && tokens[rm_idx + 1..].iter().any(|t| {
            !t.starts_with('-')
                && (matches!(*t, "." | "./" | "*")
                    || t.trim_end_matches('/') == "."
                    || delete_guard::check_recursive_rm_target(t, "rm", ctx).is_some()
                    || delete_guard::is_guarded_dir(&ctx.expand(t)))
        })
    {
        return Some("mixes recursive rm with a dangerous or out-of-repo target");
    }
    if tokens.iter().any(|t| {
        paths::is_secret_file(Path::new(t)).is_some()
            || t.split_once('=')
                .is_some_and(|(_, val)| paths::is_secret_file(Path::new(val)).is_some())
    }) {
        return Some("references a secret file");
    }
    if (has("gh") && has("pr") && has("merge"))
        || (lower.contains("/pulls/") && lower.contains("/merge"))
        || (lower.contains("pulls/") && lower.contains("/merge"))
        || lower.contains("mergepullrequest")
    {
        return Some("attempts a direct GitHub PR merge");
    }
    if tokens
        .iter()
        .any(|t| interp_guard::is_interpreter(basename(t)))
        && interp_guard::has_dangerous_api(line)
    {
        return Some("runs an inline interpreter script with a dangerous API");
    }
    if human_guard::names_action(&tokens) {
        return Some("runs aaf with a human-only action");
    }
    if data_guard::mentions(line, ctx) {
        return Some("names the AutoAgentFactory data directory");
    }
    if store_guard::matches(&tokens, true).is_some() {
        return Some("looks like a store submission");
    }
    if heavy_guard::mentions(&tokens) {
        return Some("names a heavy build tool");
    }
    None
}
