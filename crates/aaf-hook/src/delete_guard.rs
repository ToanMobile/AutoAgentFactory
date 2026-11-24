//! Protects secrets and factory settings inside the repository from being
//! deleted, moved or overwritten.

use std::fs;
use std::path::{Path, PathBuf};

use crate::shell::Cmd;
use crate::{Ctx, Denial, git_guard, paths};

mod find;
mod writers;

use find::{DELETERS, feeds_protected, find};

const RULE: &str = "delete_guard";
const FIX: &str = "leave it in place; protected files are changed by a human, by hand";
/// Directories whose removal takes protected settings with them.
const GUARDED_DIRS: &[&str] = &[".claude", ".autofactory"];
/// Protected `(parent_dir, file_name)` pairs mirrored from `paths.rs` for glob fallback.
const NESTED_PROTECTED: &[(&str, &str)] = &[
    (".autofactory", "config.toml"),
    (".claude", "settings.json"),
    (".claude", "settings.local.json"),
];
/// Additional `.env.*` variants checked during glob pattern fallback.
const ENV_SAMPLES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    ".env.development",
    ".env.staging",
    ".env.test",
    ".env.example",
    ".env.qa",
];
/// Bounds for directory walks over globs with wildcards in parent segments.
const WALK_ENTRIES: usize = 2000;
const WALK_DEPTH: usize = 8;

fn deny(reason: String) -> Denial {
    Denial::new(RULE, reason, FIX)
}

/// True when `path` (or its symlink target) names one of `GUARDED_DIRS` under
/// APFS case folding (e.g. `.claude`, `.CLAUDE`, `.autofactory`, or a symlink
/// pointing to one).
pub(crate) fn is_guarded_dir(path: &Path) -> bool {
    let matches_dir = |p: &Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| GUARDED_DIRS.contains(&paths::fold(name).as_str()))
    };
    matches_dir(path) || matches_dir(&paths::real(path))
}

/// The protected pattern of a file in the repository, by the name given or
/// by the file a symlink leads to: writing through `env_link -> .env`
/// changes `.env`.
fn protected(path: &Path, ctx: &Ctx) -> Option<&'static str> {
    let named = |path: &Path| paths::protected_name(path).filter(|_| ctx.in_repo(path));
    named(path).or_else(|| named(&paths::real(path)))
}

/// `Write|Edit|MultiEdit` on a path with `.`/`..` already removed.
pub fn check_file(path: &Path, ctx: &Ctx) -> Option<Denial> {
    let pattern = protected(path, ctx)?;
    Some(deny(format!(
        "{} is a protected file (pattern {pattern})",
        path.display()
    )))
}

pub fn check_cmd(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    for redirect in &cmd.redirects {
        if redirect.op.contains('>')
            && let Some(denial) = target(&redirect.target, false, "redirect to", ctx)
        {
            return Some(denial);
        }
    }
    let name = cmd.name();
    let args = cmd.args();
    if cmd.xargs
        && (cmd.dynamic_name || DELETERS.contains(&name))
        && feeds_protected(cmd.feeder.as_deref(), ctx)
    {
        let tool = if cmd.dynamic_name {
            "a tool chosen at run time"
        } else {
            name
        };
        return Some(Denial::new(
            RULE,
            format!("xargs would hand {tool} file names that may include protected files"),
            "feed xargs from a find whose -name pattern cannot match a protected file, or pass explicit paths",
        ));
    }
    match name {
        "rm" | "unlink" | "truncate" | "mv" | "tee" | "shred" => {
            let is_recursive_rm = name == "rm"
                && args
                    .iter()
                    .any(|a| a == "--recursive" || short_has(a, &['r', 'R']));
            let takes_dirs = name == "mv" || is_recursive_rm;
            positional(args).into_iter().find_map(|arg| {
                if is_recursive_rm && let Some(denial) = check_recursive_rm_target(arg, "rm", ctx) {
                    return Some(denial);
                }
                target(arg, takes_dirs, name, ctx)
            })
        }
        // Node's `rimraf`/`del`, the `trash` tools: always recursive.
        "rimraf" | "trash" | "trash-put" | "del" | "del-cli" => {
            positional(args).into_iter().find_map(|arg| {
                check_recursive_rm_target(arg, name, ctx).or_else(|| target(arg, true, name, ctx))
            })
        }
        "cp" | "install" | "rsync" => writers::check_copier(name, args, ctx),
        "dd" => args.iter().find_map(|arg| {
            arg.strip_prefix("of=")
                .and_then(|out| target(out, false, "dd", ctx))
        }),
        "sed" => writers::check_sed(args, ctx),
        "git" => git(args, ctx),
        "find" => find(args, ctx),
        _ => None,
    }
}

fn check_one_rm_path(
    raw: &str,
    path: &Path,
    verb: &str,
    holds_repo_ok: bool,
    ctx: &Ctx,
) -> Option<Denial> {
    let check_path = if raw.ends_with('/') {
        paths::real(path)
    } else if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        paths::real(parent).join(name)
    } else {
        paths::real(path)
    };
    if let Some(why) = paths::is_system_or_home_danger(&check_path, ctx.env.home.as_deref()) {
        return Some(deny(format!("{verb} {raw}: refuses to delete {why}")));
    }
    // The repository the call started in, or a directory above it, after a
    // `cd`/`env -C` elsewhere: it is not "inside the workspace".
    if !holds_repo_ok && (ctx.holds_workspace_repo(&check_path) || ctx.holds_workspace_repo(path)) {
        return Some(deny(format!(
            "{verb} {raw}: it contains the whole repository"
        )));
    }
    // Fast path: a target strictly inside the real workspace `workspace_cwd`
    // is already inside the repo (or workspace when outside any repo) without
    // spawning git for `repo_root`. The workspace directory itself is not
    // strictly inside it, except as a `find` root whose contents alone go.
    if !ctx.workspace_cwd.as_os_str().is_empty() {
        let real_ws = paths::real(&ctx.workspace_cwd);
        if (holds_repo_ok || check_path != real_ws) && check_path.starts_with(&real_ws) {
            return None;
        }
    }
    let repo = ctx.workspace_root();
    let in_repo = match &repo {
        Some(root) => check_path.starts_with(paths::real(root)),
        None => check_path.is_relative() && !paths::escapes(&check_path),
    };
    if !in_repo && !paths::is_allowed_tmp_subpath(raw, &check_path, repo.as_deref()) {
        return Some(deny(format!(
            "{verb} {raw}: recursive delete target lies outside the repository root (only repo-local paths or /tmp / $TMPDIR subpaths are allowed)"
        )));
    }
    None
}

/// Enforces home/system protection, unresolved-var fail-closed, and repo-root
/// containment (allowing only `/tmp/...` and `$TMPDIR/...` subpaths outside the
/// repository) for recursive deletion targets (`rm -r`/`-rf`, `shutil.rmtree`, etc.).
pub(crate) fn check_recursive_rm_target(raw: &str, verb: &str, ctx: &Ctx) -> Option<Denial> {
    contain(raw, verb, false, ctx)
}

/// [`check_recursive_rm_target`] for a directory whose contents are deleted
/// but not the directory itself (`find <root> -delete`): a root holding the
/// repository is left to the `-name` test analysis.
pub(crate) fn check_delete_root(raw: &str, verb: &str, ctx: &Ctx) -> Option<Denial> {
    contain(raw, verb, true, ctx)
}

fn contain(raw: &str, verb: &str, holds_repo_ok: bool, ctx: &Ctx) -> Option<Denial> {
    if paths::references_home(raw) && ctx.env.home.is_none() {
        return Some(deny(format!(
            "{verb} {raw}: home directory path cannot be safely verified"
        )));
    }
    if paths::has_unresolved_var(raw) {
        return Some(deny(format!(
            "{verb} {raw}: recursive delete target contains an unresolved variable or substitution"
        )));
    }
    let path = ctx.expand(raw);
    if let Some(denial) = check_one_rm_path(raw, &path, verb, holds_repo_ok, ctx) {
        return Some(denial);
    }
    if raw.contains("..") {
        let real_dd = paths::expand_real_dotdot(raw, &ctx.cwd, ctx.env.home.as_deref());
        if real_dd != path {
            return check_one_rm_path(raw, &real_dd, verb, holds_repo_ok, ctx);
        }
    }
    None
}

fn short_has(arg: &str, flags: &[char]) -> bool {
    arg.len() > 1 && arg.starts_with('-') && !arg.starts_with("--") && arg.contains(flags)
}

fn positional(args: &[String]) -> Vec<&str> {
    let mut only_positional = false;
    let mut out = Vec::new();
    for arg in args.iter().map(String::as_str) {
        if only_positional || !arg.starts_with('-') {
            out.push(arg);
        } else if arg == "--" {
            only_positional = true;
        }
    }
    out
}

fn git(args: &[String], ctx: &Ctx) -> Option<Denial> {
    let call = git_guard::split(args);
    match call.sub {
        "rm" => {
            // Pathspecs are relative to `-C <dir>` when given.
            let moved;
            let ctx = match call.dir {
                Some(dir) => {
                    moved = ctx.with_cwd(ctx.expand_real_dotdot(dir));
                    &moved
                }
                None => ctx,
            };
            positional(call.rest)
                .into_iter()
                .find_map(|arg| target(arg, true, "git rm", ctx))
        }
        "clean" => {
            let flags: Vec<&str> = call
                .rest
                .iter()
                .map(String::as_str)
                .take_while(|a| *a != "--")
                .collect();
            let ignored = flags.iter().any(|a| short_has(a, &['x', 'X']));
            // The last of `-n`/`--dry-run` and `--no-dry-run` wins, as in git.
            let dry_run = flags.iter().fold(false, |dry_run, a| {
                if *a == "--dry-run" || short_has(a, &['n']) {
                    true
                } else {
                    dry_run && !a.starts_with("--no-d")
                }
            });
            (ignored && !dry_run).then(|| {
                Denial::new(
                    RULE,
                    "git clean -x/-X deletes ignored files such as .env and keystores",
                    "use git clean -fd (without -x), or preview with -n",
                )
            })
        }
        _ => None,
    }
}

pub(crate) fn target(raw: &str, takes_dirs: bool, verb: &str, ctx: &Ctx) -> Option<Denial> {
    let path = ctx.expand(raw);
    let check = |p: &Path| {
        if paths::has_glob(&p.to_string_lossy()) {
            glob_hit(p, takes_dirs, ctx)
        } else {
            hit(p, takes_dirs, ctx)
        }
    };
    let why = check(&path)
        .or_else(|| {
            if !raw.contains("..") {
                return None;
            }
            let real_dd = paths::expand_real_dotdot(raw, &ctx.cwd, ctx.env.home.as_deref());
            (real_dd != path).then(|| check(&real_dd)).flatten()
        })
        .or_else(|| {
            (paths::has_glob(raw) && raw.contains(".."))
                .then(|| glob_dotdot_hit(raw, takes_dirs, ctx))
                .flatten()
        });
    why.map(|why| deny(format!("{verb} {raw}: {why}")))
}

fn hit(path: &Path, takes_dirs: bool, ctx: &Ctx) -> Option<String> {
    if let Some(pattern) = protected(path, ctx) {
        return Some(format!("protected file (pattern {pattern})"));
    }
    if !takes_dirs {
        return None;
    }
    if is_guarded_dir(path) && ctx.in_repo(path) {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        return Some(format!("{name}/ holds protected settings"));
    }
    // Only a path at or above the current or workspace working directory can
    // contain the repo, so the root lookup stays off the common `rm -rf build` path.
    if (paths::is_inside(&ctx.cwd, path)
        && ctx.root().is_some_and(|root| paths::is_inside(&root, path)))
        || (!ctx.workspace_cwd.as_os_str().is_empty()
            && paths::is_inside(&ctx.workspace_cwd, path)
            && ctx
                .workspace_root()
                .is_some_and(|root| paths::is_inside(&root, path)))
        || ctx.holds_workspace_repo(path)
    {
        return Some("it contains the whole repository".to_string());
    }
    None
}

#[derive(Default)]
struct Walk {
    seen: usize,
    matched: bool,
    cut: bool,
}

impl Walk {
    fn find(
        &mut self,
        dir: &Path,
        segs: &[&str],
        depth: usize,
        takes_dirs: bool,
        ctx: &Ctx,
    ) -> Option<String> {
        let (seg, rest) = segs.split_first()?;
        if *seg == "." {
            return if rest.is_empty() {
                self.step(dir, rest, depth, takes_dirs, ctx)
            } else {
                self.find(dir, rest, depth, takes_dirs, ctx)
            };
        }
        if *seg == ".." {
            if !dir.is_dir() {
                return None;
            }
            let real_dir = paths::real(dir);
            let parent = real_dir.parent().unwrap_or(&real_dir);
            return self.step(parent, rest, depth, takes_dirs, ctx);
        }
        if !paths::has_glob(seg) {
            return self.step(&dir.join(seg), rest, depth, takes_dirs, ctx);
        }
        let deep = *seg == "**";
        if deep
            && !rest.is_empty()
            && let Some(found) = self.find(dir, rest, depth, takes_dirs, ctx)
        {
            return Some(found);
        }
        for entry in fs::read_dir(dir).ok()?.flatten() {
            if depth >= WALK_DEPTH || self.seen >= WALK_ENTRIES {
                self.cut = true;
                return None;
            }
            self.seen += 1;
            let next = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let found = if deep {
                let leaf = rest
                    .is_empty()
                    .then(|| self.step(&next, rest, depth, takes_dirs, ctx));
                leaf.flatten()
                    .or_else(|| self.find(&next, segs, depth + 1, takes_dirs, ctx))
            } else if name == *seg || paths::glob_match(seg, &name) {
                self.step(&next, rest, depth + 1, takes_dirs, ctx)
            } else {
                None
            };
            if found.is_some() {
                return found;
            }
        }
        None
    }

    fn step(
        &mut self,
        path: &Path,
        rest: &[&str],
        depth: usize,
        takes_dirs: bool,
        ctx: &Ctx,
    ) -> Option<String> {
        if !rest.is_empty() {
            return self.find(path, rest, depth, takes_dirs, ctx);
        }
        fs::symlink_metadata(path).ok()?;
        self.matched = true;
        let why = hit(path, takes_dirs, ctx)?;
        let name = path.file_name()?.to_string_lossy();
        Some(format!("{name}: {why}"))
    }
}

pub(super) fn pattern_could_match(path: &Path, last: &str, takes_dirs: bool) -> bool {
    if paths::protected_name(path).is_some() {
        return true;
    }
    if paths::SAMPLE_NAMES
        .iter()
        .chain(ENV_SAMPLES)
        .any(|sample| paths::glob_match(last, sample))
    {
        return true;
    }
    if takes_dirs && GUARDED_DIRS.iter().any(|dir| paths::glob_match(last, dir)) {
        return true;
    }
    let parent_name = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .unwrap_or("");
    NESTED_PROTECTED
        .iter()
        .any(|(dir, file)| paths::glob_match(parent_name, dir) && paths::glob_match(last, file))
}

/// Walks unnormalized `raw` containing both a glob and `..` (e.g.
/// `claude*/../settings.json` with `claudelink -> .claude/sub`) so symlinks
/// matched by a glob are resolved before stepping into `..`.
fn glob_dotdot_hit(raw: &str, takes_dirs: bool, ctx: &Ctx) -> Option<String> {
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
    if segs.is_empty() {
        return None;
    }
    let base = ctx.expand_real_dotdot(prefix.to_str()?);
    let mut walk = Walk::default();
    walk.find(&base, &segs, 0, takes_dirs, ctx)
}

/// Expands a glob against the real directory tree (resolving symlinks and nested
/// protected files); when a parent-glob matches nothing on disk or the walk is
/// cut, falls back to pattern matching over protected names, nested pairs,
/// guarded directories, and `.env.*` variants.
fn glob_hit(path: &Path, takes_dirs: bool, ctx: &Ctx) -> Option<String> {
    let parent = path.parent()?;
    let last = path.file_name()?.to_str()?;
    if paths::has_glob(&parent.to_string_lossy()) || last == "**" {
        let mut prefix = PathBuf::new();
        let mut segs: Vec<&str> = Vec::new();
        for part in path.components() {
            let text = part.as_os_str().to_str()?;
            if segs.is_empty() && !paths::has_glob(text) {
                prefix.push(text);
            } else {
                segs.push(text);
            }
        }
        let mut walk = Walk::default();
        if let Some(found) = walk.find(&prefix, &segs, 0, takes_dirs, ctx) {
            return Some(found);
        }
        if !walk.cut && walk.matched {
            return None;
        }
        return (pattern_could_match(path, last, takes_dirs) && ctx.in_repo(path))
            .then(|| "the pattern can match a protected file".to_string());
    }
    // A directory that cannot be listed has nothing the glob could delete.
    let entries = fs::read_dir(parent).ok()?;
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| paths::glob_match(last, name))
        .find_map(|name| {
            hit(&parent.join(&name), takes_dirs, ctx).map(|why| format!("{name}: {why}"))
        })
}
