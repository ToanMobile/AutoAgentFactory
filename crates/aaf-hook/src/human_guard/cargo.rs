//! `cargo run` inspection for `human_guard`: detects when `cargo run` targets
//! `aaf-cli` and extracts the forwarded CLI arguments.

use std::fs;
use std::path::Path;

use crate::{Ctx, paths};

/// Top-level `aaf` subcommands that include at least one guarded action.
const TOP_LEVEL_ACTIONS: &[&str] = &[
    "approve", "board", "cockpit", "run", "init", "plan", "gc", "merge", "proof", "skills", "sop",
    "telegram", "ui", "update", "upgrade",
];

/// Upper bound when inspecting a `Cargo.toml` to see whether `cargo run` targets `aaf-cli`.
const MAX_CARGO_TOML_BYTES: u64 = 256 * 1024;

fn find_subcommand_pos(rest: &[String]) -> Option<usize> {
    let mut i = 0;
    while let Some(arg) = rest.get(i) {
        if matches!(
            arg.as_str(),
            "-p" | "--package"
                | "--bin"
                | "--example"
                | "--manifest-path"
                | "--target"
                | "--target-dir"
                | "--profile"
                | "-F"
                | "--features"
        ) {
            i += 2;
            continue;
        }
        if TOP_LEVEL_ACTIONS.contains(&arg.as_str()) {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn toml_mentions_aaf_cli(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() > MAX_CARGO_TOML_BYTES {
        return false;
    }
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    if text.contains("aaf-cli") {
        return true;
    }
    text.lines().any(|line| {
        let trimmed = line.trim();
        let Some((k, v)) = trimmed.split_once('=') else {
            return false;
        };
        let k = k.trim();
        let v = v.split('#').next().unwrap_or("").trim();
        k == "name" && (v == "\"aaf\"" || v == "'aaf'")
    })
}

fn cargo_workspace_has_aaf(ctx: &Ctx, manifest_path: Option<&str>) -> bool {
    if let Some(mp) = manifest_path {
        if mp.is_empty() || mp.contains(['$', '`']) || paths::has_glob(mp) {
            return true;
        }
        return toml_mentions_aaf_cli(&ctx.expand(mp));
    }
    let root = ctx.root();
    if let Some(root_dir) = &root
        && toml_mentions_aaf_cli(&root_dir.join("Cargo.toml"))
    {
        return true;
    }
    for dir in ctx.cwd.ancestors().take(8) {
        if toml_mentions_aaf_cli(&dir.join("Cargo.toml")) {
            return true;
        }
        if root.as_deref() == Some(dir) {
            break;
        }
    }
    false
}

/// `cargo run [-p aaf-cli] -- <aaf args>`: the same CLI, built on the spot.
/// Triggers only when `-p`/`--package`/`--bin` (or `--manifest-path`) names
/// `aaf*` OR the workspace `Cargo.toml` at `ctx.root()`/`ctx.cwd` actually
/// defines or mentions `aaf-cli`, avoiding false positives in non-aaf Rust repos.
pub(super) fn cargo_run_args<'a>(args: &'a [String], ctx: &Ctx) -> Option<(&'a [String], bool)> {
    let run = args.iter().position(|arg| arg == "run" || arg == "r")?;
    let rest = args.get(run + 1..)?;
    let split = match rest.iter().position(|arg| arg == "--") {
        Some(dashes) => (dashes, dashes + 1),
        // cargo hands the words after its options to the program without
        // `--` as well; option values make the first of them uncertain, so
        // the search starts at the word that would name a guarded action.
        None => {
            let at = find_subcommand_pos(rest)?;
            (at, at)
        }
    };
    let (options, passed) = (rest.get(..split.0)?, rest.get(split.1..)?);
    let mut selected = Vec::new();
    let mut manifest_path: Option<&str> = None;
    let mut at = 0;
    while let Some(option) = options.get(at).map(String::as_str) {
        at += 1;
        if matches!(option, "-p" | "--package" | "--bin") {
            selected.push(options.get(at).map_or("", String::as_str));
            at += 1;
        } else if option == "--manifest-path" {
            manifest_path = Some(options.get(at).map_or("", String::as_str));
            at += 1;
        } else if let Some(mp) = option.strip_prefix("--manifest-path=") {
            manifest_path = Some(mp);
        } else if let Some(glued) = ["--package=", "--bin=", "-p"]
            .iter()
            .find_map(|prefix| option.strip_prefix(prefix))
        {
            selected.push(glued);
        }
    }
    let mut before = args.iter().take(run + 1 + split.0);
    let names_aaf = before.any(|arg| arg.contains("aaf"));
    let unclear = |name: &&str| name.is_empty() || name.contains(['$', '`', '*', '?']);
    let other = !selected.is_empty() && !names_aaf && !selected.iter().any(unclear);
    if other {
        return None;
    }
    if names_aaf || cargo_workspace_has_aaf(ctx, manifest_path) {
        // Without aaf explicitly named on the line, only literal actions count.
        Some((passed, names_aaf))
    } else {
        None
    }
}
