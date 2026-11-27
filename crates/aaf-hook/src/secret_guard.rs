//! Blocks reading or exfiltrating secret files (`.env`, `.env.*` except public
//! templates, keystores, `keystore.properties`, `google-services.json`, PEM/P12,
//! SSH private keys) through both the `Read` file tool and `Bash` file-reading
//! commands or input redirections.

use std::fs;
use std::path::{Path, PathBuf};

use crate::shell::Cmd;
use crate::{Ctx, Denial, paths};

pub const RULE: &str = "secret_guard";
const FIX: &str = "do not read secret/credential files; inspect .env.example, .env.sample, \
                   .env.template, or mock/test fixtures instead; this needs a human: finish the \
                   task without it and name the step in your final message";

fn deny(reason: String) -> Denial {
    Denial::new(RULE, reason, FIX)
}

/// Checks `path` and its symlink target against the secret file patterns.
pub fn secret_pattern(path: &Path) -> Option<&'static str> {
    paths::is_secret_file(path).or_else(|| paths::is_secret_file(&paths::real(path)))
}

/// Guard for the `Read` file tool.
pub fn check_read(path: &Path) -> Option<Denial> {
    let pattern = secret_pattern(path)?;
    Some(deny(format!(
        "reading {} is blocked (secret file pattern {pattern})",
        path.display()
    )))
}

/// Commands whose non-option operands are file paths being read, copied, or printed.
const READERS: &[&str] = &[
    "cat", "head", "tail", "less", "more", "bat", "sed", "awk", "gawk", "mawk", "nawk", "grep",
    "egrep", "fgrep", "rg", "strings", "base64", "xxd", "hexdump", "od", "nl", "tac", "rev",
    "pbcopy", "tee", "cp", "dd", "cut", "sort", "uniq", "wc", "diff", "cmp", "source", ".",
];

/// Commands whose first positional operand is a pattern/script rather than a file
/// unless `-e`/`-f`/`--regexp`/`--file` was supplied.
const FIRST_POS_IS_EXPR: &[&str] = &[
    "grep", "egrep", "fgrep", "rg", "sed", "awk", "gawk", "mawk", "nawk",
];

/// Options of file-reading commands that take a separate non-file argument value.
/// Short options of commands with a [`Spec`] are parsed by [`Spec::classify`].
/// An option is listed only if it takes a REQUIRED value on GNU and BSD alike
/// (BSD grep's `--context` and BSD sed's `-l` take none): a wrong entry would
/// swallow the operand after it.
fn option_takes_value(cmd: &str, opt: &str) -> bool {
    match cmd {
        "head" | "tail" => matches!(opt, "-n" | "-c" | "--lines" | "--bytes"),
        "grep" | "egrep" | "fgrep" | "rg" => matches!(
            opt,
            "--max-count"
                | "--after-context"
                | "--before-context"
                | "--glob"
                | "--type"
                | "--include"
                | "--exclude"
        ),
        "cut" => matches!(opt, "-d" | "-f" | "-c" | "-b"),
        "sort" => matches!(opt, "-t" | "-k" | "-o"),
        _ => false,
    }
}

/// What a recognised option's value is.
#[derive(Clone, Copy)]
enum Kind {
    /// A pattern/script (grep/rg/sed only): the one value never checked as a file.
    Expr,
    /// A file that is read; also marks the first positional as a file operand.
    File,
    /// Any other value; still checked, since swallowing it unchecked could hide a file.
    Value,
}

/// getopt-style grammar of commands that accept inline values: clustered short
/// flags (`-ieKEY`, `-if.env`, `-di.env`) and abbreviated long options
/// (`--reg=KEY`, `--in=.env`). Unlisted letters are treated as boolean flags;
/// grep/rg/sed reject unknown ones, but BSD awk ignores a whole unknown token
/// (`-xv`), so every awk value letter is `File` (checked, operands explicit).
struct Spec {
    expr: &'static str,
    file: &'static str,
    value: &'static str,
    /// Letters whose value can only be attached (`sed -i[SUFFIX]`): never
    /// consume the next token, so `sed -i 'p' .env` still checks `.env`.
    attached: &'static str,
    expr_long: &'static [&'static str],
    file_long: &'static [&'static str],
}

fn spec(cmd: &str) -> Option<Spec> {
    let s = |expr, file, value, attached, expr_long, file_long| {
        Some(Spec {
            expr,
            file,
            value,
            attached,
            expr_long,
            file_long,
        })
    };
    match cmd {
        "grep" | "egrep" | "fgrep" => s("e", "f", "mABCdD", "", &["regexp"], &["file"]),
        "rg" => s("e", "f", "ABCEMTdgjmrt", "", &["regexp"], &["file"]),
        "sed" => s("e", "f", "", "iI", &["expression"], &["file"]),
        // BSD awk ignores unknown flags (`-e x` leaves `x` as an operand), so
        // awk program text is checked like a file too.
        "awk" | "gawk" | "mawk" | "nawk" => s(
            "",
            "efEiFv",
            "",
            "",
            &[],
            &["source", "file", "exec", "include"],
        ),
        // `-i`/`--input` is the input file on BSD/macOS (`--ignore-garbage` on GNU).
        "base64" => s("", "i", "bow", "", &[], &["input"]),
        _ => None,
    }
}

impl Spec {
    /// Classifies an option token as `(kind, inline value)`; an inline value of
    /// `None` means the value is the next argument. Unknown long options return
    /// `None`; a short cluster of boolean flags is `Value` with an empty value.
    fn classify<'a>(&self, arg: &'a str) -> Option<(Kind, Option<&'a str>)> {
        if let Some(long) = arg.strip_prefix("--") {
            let (name, val) = long
                .split_once('=')
                .map_or((long, None), |(n, v)| (n, Some(v)));
            // getopt_long accepts any unambiguous prefix: `--reg`, `--in`, `--expr`.
            let hit =
                |longs: &[&str]| !name.is_empty() && longs.iter().any(|l| l.starts_with(name));
            return if hit(self.expr_long) {
                Some((Kind::Expr, val))
            } else if hit(self.file_long) {
                Some((Kind::File, val))
            } else {
                None
            };
        }
        let mut chars = arg.chars();
        chars.next();
        while let Some(c) = chars.next() {
            let rest = chars.as_str();
            let inline = (!rest.is_empty()).then_some(rest);
            if self.expr.contains(c) {
                return Some((Kind::Expr, inline));
            } else if self.file.contains(c) {
                return Some((Kind::File, inline));
            } else if self.value.contains(c) {
                return Some((Kind::Value, inline));
            } else if self.attached.contains(c) {
                return Some((Kind::Value, Some(rest)));
            }
        }
        Some((Kind::Value, Some("")))
    }
}

/// Names a glob could expand to, one per secret file pattern.
const SECRET_SAMPLES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    ".env.development",
    ".env.staging",
    ".env.test",
    "a.keystore",
    "a.jks",
    "a.p12",
    "a.mobileprovision",
    "keystore.properties",
    "google-services.json",
    "GoogleService-Info.plist",
    "id_rsa",
    "id_rsa.bak",
    "id_ed25519",
    ".env.qa",
    "server.pem",
];

/// Entries and directory levels one glob walk may list, so the hook stays
/// fast and a symlink cycle ends; a cut walk falls back to the samples.
const WALK_ENTRIES: usize = 2000;
const WALK_DEPTH: usize = 8;

/// Lists the tree for what a glob's segments name, like the shell would.
#[derive(Default)]
struct Walk {
    seen: usize,
    matched: bool,
    cut: bool,
}

impl Walk {
    /// The first secret file `segs` name under `dir`. `**` spans zero or more
    /// directories; `[id]` names any one character or the literal `[id]`.
    fn find(&mut self, dir: &Path, segs: &[&str], depth: usize) -> Option<String> {
        let (seg, rest) = segs.split_first()?;
        if !paths::has_glob(seg) {
            return self.step(&dir.join(seg), rest, depth);
        }
        let deep = *seg == "**";
        if deep
            && !rest.is_empty()
            && let Some(hit) = self.find(dir, rest, depth)
        {
            return Some(hit);
        }
        for entry in fs::read_dir(dir).ok()?.flatten() {
            if depth >= WALK_DEPTH || self.seen >= WALK_ENTRIES {
                self.cut = true;
                return None;
            }
            self.seen += 1;
            let next = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let hit = if deep {
                let leaf = rest.is_empty().then(|| self.step(&next, rest, depth));
                leaf.flatten().or_else(|| self.find(&next, segs, depth + 1))
            } else if name == *seg || paths::glob_match(seg, &name) {
                self.step(&next, rest, depth + 1)
            } else {
                None
            };
            if hit.is_some() {
                return hit;
            }
        }
        None
    }

    /// `path` was named: judge it (symlinks resolved), or walk on below it.
    fn step(&mut self, path: &Path, rest: &[&str], depth: usize) -> Option<String> {
        if !rest.is_empty() {
            return self.find(path, rest, depth);
        }
        fs::symlink_metadata(path).ok()?;
        self.matched = true;
        let pat = secret_pattern(path)?;
        let name = path.file_name()?.to_string_lossy();
        Some(format!("{name} (pattern {pat})"))
    }
}

/// The secret file the glob `raw` can expand to, as `"<name> (pattern <pat>)"`,
/// walking the tree from `raw`'s literal prefix. A cut walk, or a directory
/// glob naming nothing on disk, judges the last segment by name instead.
fn glob_secret(raw: &str, ctx: &Ctx) -> Option<String> {
    let mut prefix = PathBuf::new();
    let mut segs: Vec<&str> = Vec::new();
    for part in Path::new(raw).components() {
        let text = part.as_os_str().to_str()?;
        if segs.is_empty() && !paths::has_glob(text) {
            prefix.push(text);
        } else {
            segs.push(text);
        }
    }
    let last = *segs.last()?;
    let prefix_str = prefix.to_str()?;
    let mut walk = Walk::default();
    if let Some(hit) = walk.find(&ctx.expand_real_dotdot(prefix_str), &segs, 0) {
        return Some(hit);
    }
    if !walk.cut && (walk.matched || segs.len() == 1 && !last.contains("**")) {
        return None;
    }
    if !paths::has_glob(last) {
        let pat = secret_pattern(&ctx.expand(raw)).or_else(|| {
            raw.contains("..")
                .then(|| secret_pattern(&ctx.expand_real_dotdot(raw)))
                .flatten()
        })?;
        return Some(format!("{last} (pattern {pat})"));
    }
    if walk.cut
        && let Some(pat) = paths::may_name_secret(last)
    {
        return Some(format!("{last} (pattern {pat})"));
    }
    SECRET_SAMPLES.iter().find_map(|sample| {
        let pat = paths::is_secret_file(Path::new(sample))?;
        paths::glob_match(last, sample).then(|| format!("{sample} (pattern {pat})"))
    })
}

/// Guard for a globbed read target `raw`, shared by the `Bash` readers and
/// the read file tools.
pub fn check_glob(raw: &str, ctx: &Ctx, verb: &str) -> Option<Denial> {
    let hit = glob_secret(raw, ctx)?;
    Some(deny(format!(
        "{verb} {raw}: glob matches secret file {hit}"
    )))
}

/// Guard for the read file tools (`Read`, Gemini `read_file` and
/// `read_many_files`): the path itself, then what a glob in it can expand to.
pub fn check_read_target(raw: &str, path: &Path, ctx: &Ctx) -> Option<Denial> {
    check_read(path)
        .or_else(|| {
            if !raw.contains("..") {
                return None;
            }
            let real_dd = ctx.expand_real_dotdot(raw);
            (real_dd != path).then(|| check_read(&real_dd)).flatten()
        })
        .or_else(|| {
            paths::has_glob(raw)
                .then(|| check_glob(raw, ctx, "read"))
                .flatten()
        })
}

fn check_target(raw: &str, verb: &str, ctx: &Ctx) -> Option<Denial> {
    if raw.is_empty() || raw == "-" {
        return None;
    }
    if paths::has_glob(raw) {
        return check_glob(raw, ctx, verb);
    }
    let path = ctx.expand(raw);
    let pat = secret_pattern(&path).or_else(|| {
        if !raw.contains("..") {
            return None;
        }
        let real_dd = ctx.expand_real_dotdot(raw);
        (real_dd != path)
            .then(|| secret_pattern(&real_dd))
            .flatten()
    })?;
    Some(deny(format!(
        "{verb} {raw}: reading secret file is blocked (pattern {pat})"
    )))
}

/// Guard for `Bash` commands reading secret files via operands or `<` redirections.
pub fn check_cmd(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    // 1. Check input redirections (`< .env`, `<> .env`) on ANY command (e.g. `pbcopy < .env`).
    for redirect in &cmd.redirects {
        if matches!(redirect.op.as_str(), "<" | "<>")
            && let Some(denial) = check_target(&redirect.target, "redirect from", ctx)
        {
            return Some(denial);
        }
    }

    let name = cmd.name();
    if !READERS.contains(&name) {
        return None;
    }

    let args = cmd.args();
    // Special case: `dd if=<path>`
    if name == "dd" {
        for arg in args {
            if let Some(target) = arg.strip_prefix("if=")
                && let Some(denial) = check_target(target, "dd", ctx)
            {
                return Some(denial);
            }
        }
        return None;
    }

    let spec = spec(name);
    let mut has_explicit_expr = false;
    let mut positionals: Vec<&str> = Vec::new();
    let mut only_pos = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if only_pos {
            positionals.push(arg);
            i += 1;
            continue;
        }
        if arg == "--" {
            only_pos = true;
            i += 1;
            continue;
        }
        let is_opt = arg.starts_with('-') && arg.len() > 1;
        if is_opt && let Some((kind, inline)) = spec.as_ref().and_then(|s| s.classify(arg)) {
            let value = inline.or_else(|| args.get(i + 1).map(String::as_str));
            i += if inline.is_some() { 1 } else { 2 };
            if !matches!(kind, Kind::Value) {
                has_explicit_expr = true;
            }
            // BSD getopt stops at the first operand: `sed p -e .env` reads `.env`.
            if (!matches!(kind, Kind::Expr) || !positionals.is_empty())
                && let Some(denial) = value.and_then(|v| check_target(v, name, ctx))
            {
                return Some(denial);
            }
            continue;
        }
        if is_opt {
            // Swallowed values are still checked (`tail -f .env`, and BSD reads
            // `.env` as a file in `head README.md -n .env`).
            let takes = !arg.contains('=')
                && (option_takes_value(name, arg) || arg == "-f" || arg == "--file");
            if takes && let Some(d) = args.get(i + 1).and_then(|v| check_target(v, name, ctx)) {
                return Some(d);
            }
            i += if takes { 2 } else { 1 };
            continue;
        }
        positionals.push(arg);
        i += 1;
    }

    if name == "sed" {
        for read_path in crate::sed_script::script_files(args, &['r', 'R']) {
            if let Some(denial) = check_target(&read_path, name, ctx) {
                return Some(denial);
            }
        }
    }

    let skip_first = FIRST_POS_IS_EXPR.contains(&name) && !has_explicit_expr;
    let file_args = if skip_first && !positionals.is_empty() {
        &positionals[1..]
    } else {
        &positionals[..]
    };

    file_args
        .iter()
        .find_map(|arg| check_target(arg, name, ctx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sample_is_a_secret_file() {
        for sample in SECRET_SAMPLES {
            assert!(
                paths::is_secret_file(Path::new(sample)).is_some(),
                "{sample} is not caught by is_secret_file"
            );
        }
    }
}
