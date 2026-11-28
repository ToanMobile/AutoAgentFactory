//! Inspects inline scripts passed to interpreters (`python`/`python3 -c`,
//! `node -e`, `perl -e`, `ruby -e`, `php -r`) so dangerous shell/delete/git
//! operations and secret file reads cannot bypass `aaf-hook`.

use std::collections::HashSet;
use std::path::Path;

use crate::shell::{self, Cmd};
use crate::{Ctx, Denial, data_guard, delete_guard, git_files, paths, secret_guard};

mod comments;
mod parse;
mod scope;
mod writes;

use comments::{strip_comment_lines, strip_comments_quote_aware, strip_trailing_comments};

use parse::{
    Interp, PerlSwitches, classify_interpreter, extract_delimited_commands, extract_literals,
    extract_space_call_commands, has_space_call, inline_payloads, interp_spec,
    next_space_delimited_cluster, normalize_api_haystack, perl_switch_value,
};
use scope::{
    extract_api_arg_slices, has_any, has_shell_exec_api, is_safe_py_mktemp_arg, safe_py_mktemp_vars,
};

pub const RULE: &str = "interp_guard";
const FIX_RMTREE: &str = "use an explicit literal path under the repository or /tmp";

/// True when `name` is a supported interpreter (including versioned forms like `python3.12`).
pub(crate) fn is_interpreter(name: &str) -> bool {
    classify_interpreter(name).is_some()
}

/// A `perl -e`/`-E` script among `args` opens a file for writing (or
/// `sysopen`s one): it does not only read the files it is handed.
pub(crate) fn perl_inline_writes(args: &[String]) -> bool {
    inline_payloads(Interp::Perl, args)
        .iter()
        .any(|code| writes::writes_files(code))
}

/// Options that name the program another way than standard input: Python's
/// `-m module`, PHP's `-f file` / `-F file`.
fn names_program(interp: Interp, flag: &str) -> bool {
    match interp {
        Interp::Python => flag == "m",
        Interp::Php => matches!(flag, "f" | "F" | "file" | "process-file"),
        _ => false,
    }
}

/// The interpreter `cmd` runs reads its program from standard input: no
/// inline code, no module or script file, or the script named `-`. Only
/// then is a quoted heredoc on it the program and not data a program could
/// hand to a shell (`perl -e 'exec "sh"' <<'EOF'`).
pub(crate) fn reads_program_from_stdin(cmd: &Cmd) -> bool {
    let Some(interp) = classify_interpreter(cmd.name()) else {
        return false;
    };
    let args = cmd.args();
    if cmd.dynamic_name || !inline_payloads(interp, args).is_empty() {
        return false;
    }
    let spec = interp_spec(interp);
    let mut i = 0;
    while let Some(arg) = args.get(i).map(String::as_str) {
        if arg == "--" {
            return args.get(i + 1).is_none_or(|op| op == "-");
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (flag, glued) = long
                .split_once('=')
                .map_or((long, None), |(f, v)| (f, Some(v)));
            if names_program(interp, flag) {
                return false;
            }
            i += if glued.is_none() && spec.other_valued_long.contains(&flag) {
                2
            } else {
                1
            };
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            let mut chars = arg.chars();
            chars.next();
            let mut step = 1;
            while let Some(c) = chars.next() {
                let mut buf = [0; 4];
                if names_program(interp, c.encode_utf8(&mut buf)) {
                    return false;
                }
                let rest = chars.as_str();
                if interp == Interp::Perl
                    && perl_switch_value(c, rest, &mut PerlSwitches::default())
                {
                    break;
                }
                if spec.other_valued_short.contains(c) {
                    if rest.is_empty() {
                        step = 2;
                    }
                    break;
                }
                if spec.attached_one_char_short.contains(c) {
                    let _ = chars.next();
                    continue;
                }
                if spec.attached_space_delimited_short.contains(c) {
                    if let Some(next_cluster) = next_space_delimited_cluster(rest) {
                        chars = next_cluster.chars();
                        continue;
                    }
                    break;
                }
                if spec.attached_only_short.contains(c) {
                    break;
                }
            }
            i += step;
            continue;
        }
        return arg == "-";
    }
    true
}

fn deny(reason: String, fix: &str) -> Denial {
    Denial::new(RULE, reason, fix)
}

const RMTREE_APIS: &[&str] = &[
    "shutil.rmtree",
    "rmtree(",
    "rmSync(",
    "fs.rm(",
    "rmdirSync(",
    "FileUtils.rm_rf",
    "FileUtils.rm_r",
    "remove_tree(",
];
const FILE_DELETE_APIS: &[&str] = &[
    "os.unlink(",
    "os.remove(",
    "os.rmdir(",
    "os.rename(",
    "os.replace(",
    ".unlink(",
];
const FILE_READ_APIS: &[&str] = &[
    "open(",
    "fopen(",
    "Path(",
    "readFileSync(",
    "readFile(",
    "File.read(",
    "File.readlines(",
    "File.binread(",
    "IO.read(",
    "IO.readlines(",
    "IO.binread(",
    "file_get_contents(",
    "readfile(",
    "file(",
    "parse_ini_file(",
    "highlight_file(",
    "show_source(",
];
const PERL_RUBY_SPACE_READ_APIS: &[&str] = &[
    "open",
    "File.read",
    "File.open",
    "File.readlines",
    "File.binread",
    "IO.read",
    "IO.readlines",
    "IO.binread",
];
const PHP_INCLUDE_KEYWORDS: &[&str] = &["include_once", "include", "require_once", "require"];

fn check_protected_path(
    trimmed: &str,
    takes_dirs: bool,
    interp: &str,
    ctx: &Ctx,
) -> Option<Denial> {
    if let Some(d) = delete_guard::target(trimmed, takes_dirs, interp, ctx) {
        return Some(deny(
            format!("{interp} delete of protected path blocked: {}", d.reason),
            &d.fix,
        ));
    }
    let check_git_and_data = |expanded: &Path| {
        if let Some(d) = git_files::check_file(expanded) {
            return Some(deny(
                format!("{interp} delete of git file blocked: {}", d.reason),
                &d.fix,
            ));
        }
        data_guard::check_file(trimmed, expanded, ctx).map(|d| {
            deny(
                format!("{interp} delete of data dir blocked: {}", d.reason),
                &d.fix,
            )
        })
    };
    let expanded = ctx.expand(trimmed);
    if let Some(d) = check_git_and_data(&expanded) {
        return Some(d);
    }
    if trimmed.contains("..") {
        let real_dd = ctx.expand_real_dotdot(trimmed);
        if real_dd != expanded {
            return check_git_and_data(&real_dd);
        }
    }
    None
}

fn check_secret_read_literal(trimmed: &str, interp: &str, ctx: &Ctx) -> Option<Denial> {
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    let expanded = ctx.expand(trimmed);
    let pat = secret_guard::secret_pattern(&expanded)
        .or_else(|| {
            if !trimmed.contains("..") {
                return None;
            }
            let real_dd = ctx.expand_real_dotdot(trimmed);
            (real_dd != expanded)
                .then(|| secret_guard::secret_pattern(&real_dd))
                .flatten()
        })
        .or_else(|| paths::is_secret_file(Path::new(trimmed)));
    pat.map(|pat| {
        deny(
            format!("{interp} attempts to read secret file `{trimmed}` (pattern {pat})"),
            "do not read secret/credential files from inline scripts; use .env.example",
        )
    })
}

fn inspect_rmtree(
    interp_kind: Interp,
    interp: &str,
    active: &str,
    hay: &str,
    ctx: &Ctx,
) -> Option<Denial> {
    let (slices, uncalled) = extract_api_arg_slices(hay, RMTREE_APIS, true);
    if slices.is_empty() && !uncalled {
        return None;
    }
    let safe_mktemp = if interp_kind == Interp::Python {
        safe_py_mktemp_vars(active)
    } else {
        HashSet::new()
    };
    for arg_slice in &slices {
        if interp_kind == Interp::Python && is_safe_py_mktemp_arg(arg_slice, &safe_mktemp) {
            continue;
        }
        let mut saw_lit = false;
        for lit in extract_literals(arg_slice) {
            let trimmed = lit.trim();
            if trimmed.is_empty()
                || matches!(
                    trimmed,
                    "utf8" | "utf-8" | "r" | "w" | "rb" | "fs" | "fileutils"
                )
            {
                continue;
            }
            saw_lit = true;
            if let Some(d) = delete_guard::check_recursive_rm_target(trimmed, interp, ctx) {
                return Some(deny(
                    format!("{interp} recursive delete blocked: {}", d.reason),
                    &d.fix,
                ));
            }
            if let Some(d) = check_protected_path(trimmed, true, interp, ctx) {
                return Some(d);
            }
        }
        if !saw_lit {
            return Some(deny(
                format!("{interp} calls a recursive delete API on an unresolved dynamic path"),
                FIX_RMTREE,
            ));
        }
    }
    uncalled.then(|| {
        deny(
            format!("{interp} references a recursive delete API indirectly"),
            FIX_RMTREE,
        )
    })
}

fn inspect_text(interp_kind: Interp, interp: &str, text: &str, ctx: &Ctx) -> Option<Denial> {
    // 0. The data directory, named anywhere in the script (a heredoc body
    // is not among the command's words that `data_dir_guard` reads), and
    // file writes into protected/git paths.
    if let Some(d) = data_guard::check_text(interp, text, ctx) {
        return Some(d);
    }
    if let Some(d) = writes::check_written(interp, text, ctx) {
        return Some(d);
    }

    // 1. Scoped recursive delete APIs (`shutil.rmtree`, `fs.rmSync`, `FileUtils.rm_rf`, ...).
    let api_haystack = normalize_api_haystack(text, interp_kind == Interp::Php);
    if let Some(d) = inspect_rmtree(interp_kind, interp, text, &api_haystack, ctx) {
        return Some(d);
    }
    let literals = extract_literals(text);

    // 2. Single-file delete/rename APIs.
    if has_any(&api_haystack, FILE_DELETE_APIS) {
        let takes_dirs = has_any(&api_haystack, &["os.rmdir(", "os.rename(", "os.replace("]);
        for trimmed in literals
            .iter()
            .map(|l| l.trim())
            .filter(|t| !t.is_empty() && !t.contains('\n'))
        {
            if let Some(d) = check_protected_path(trimmed, takes_dirs, interp, ctx) {
                return Some(d);
            }
        }
    }

    // 3. Shell-execution APIs (exempting ONLY JS regex-literal `/.../.exec(`).
    let perl_ruby = matches!(interp_kind, Interp::Perl | Interp::Ruby);
    if has_shell_exec_api(&api_haystack) || (perl_ruby && has_space_call(text, &["system", "exec"]))
    {
        if ctx.depth >= shell::MAX_DEPTH {
            return Some(deny(
                format!("{interp} shell execution is nested too deeply to analyse"),
                "rewrite as plain commands without nested interpreters",
            ));
        }
        let mut candidates: Vec<String> = literals;
        candidates.extend(extract_delimited_commands(text, '[', ']'));
        candidates.extend(extract_delimited_commands(text, '(', ')'));
        if perl_ruby {
            candidates.extend(extract_space_call_commands(text));
        }
        for snippet in &candidates {
            if let Some(d) = crate::bash(snippet, ctx.nested()) {
                return Some(deny(
                    format!(
                        "{interp} wrapped blocked command ({}): {}",
                        d.rule, d.reason
                    ),
                    &d.fix,
                ));
            }
        }
    }

    // 4. Secret file reads scoped to each matched file-read call's arguments.
    let (mut read_slices, _) = extract_api_arg_slices(&api_haystack, FILE_READ_APIS, false);
    if interp_kind == Interp::Php {
        read_slices.extend(extract_api_arg_slices(&api_haystack, PHP_INCLUDE_KEYWORDS, true).0);
    }
    if perl_ruby {
        read_slices.extend(extract_api_arg_slices(text, PERL_RUBY_SPACE_READ_APIS, true).0);
    }
    read_slices.into_iter().find_map(|slice| {
        extract_literals(slice)
            .into_iter()
            .find_map(|lit| check_secret_read_literal(lit.trim(), interp, ctx))
    })
}

fn inspect_payload(interp_kind: Interp, interp: &str, code: &str, ctx: &Ctx) -> Option<Denial> {
    if let Some(d) = inspect_text(interp_kind, interp, code, ctx) {
        return Some(d);
    }
    let stripped = strip_comment_lines(code, interp_kind);
    if stripped != code
        && let Some(d) = inspect_text(interp_kind, interp, &stripped, ctx)
    {
        return Some(d);
    }
    // SEC-113: additive quote-aware comment stripping pass so unmatched quotes
    // inside inline/block/PHP-closed comments cannot hide following code while
    // keeping `#` and `//` inside string literals intact.
    let no_comments = strip_comments_quote_aware(code, interp_kind);
    if no_comments != code
        && no_comments != stripped
        && let Some(d) = inspect_text(interp_kind, interp, &no_comments, ctx)
    {
        return Some(d);
    }
    // SEC-113 (F14): additive naive trailing-comment fallback when a literal
    // `/*` (e.g. inside a JS regex `/a\/*/`) misleads the block-comment scan.
    let no_trailing = strip_trailing_comments(code, interp_kind);
    if no_trailing != code && no_trailing != stripped && no_trailing != no_comments {
        return inspect_text(interp_kind, interp, &no_trailing, ctx);
    }
    None
}

/// True when `code` mentions any shell-exec, recursive-delete, file-delete, or
/// file-read API guarded by [`inspect_payload`].
pub(crate) fn has_dangerous_api(code: &str) -> bool {
    let norm = normalize_api_haystack(code, false);
    let lower = normalize_api_haystack(code, true);
    let check = |hay: &str| {
        has_shell_exec_api(hay)
            || has_any(hay, RMTREE_APIS)
            || has_any(hay, FILE_DELETE_APIS)
            || has_any(hay, FILE_READ_APIS)
    };
    check(&norm)
        || check(&lower)
        || has_space_call(code, &["system", "exec"])
        || has_space_call(code, PERL_RUBY_SPACE_READ_APIS)
}

pub fn check_cmd(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    let name = cmd.name();
    let interp = classify_interpreter(name)?;
    inline_payloads(interp, cmd.args())
        .into_iter()
        .chain(shell::stdin_scripts(&cmd.redirects, cmd.feeder.as_deref()))
        .chain(cmd.heredocs.iter().cloned())
        .find_map(|payload| inspect_payload(interp, name, &payload, ctx))
}
