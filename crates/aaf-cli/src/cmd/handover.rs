//! `aaf handover <task_id> [--format md|html|json] [--out <path>] [--open]`:
//! generate a read-only acceptance handover report from the database and
//! verified signed artifacts.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use aaf_domain::config::RepoConfig;
use aaf_domain::ports::{PortError, TaskStore};
use aaf_engine::db::private;
use aaf_engine::git::worktree::validate_task_id;
use aaf_engine::handover::{load_handover, render_html, render_json, render_markdown};
use serde_json::json;

use super::board::store::{err, open_db_read_only, refuse_inside_agent_run};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

static TEMP_OUT_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum HandoverFormat {
    Md,
    Html,
    Json,
}

impl HandoverFormat {
    fn as_str(self) -> &'static str {
        match self {
            HandoverFormat::Md => "md",
            HandoverFormat::Html => "html",
            HandoverFormat::Json => "json",
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct HandoverArgs {
    /// Task id (`[A-Za-z0-9_-]{1,64}`) to summarize.
    pub task_id: String,
    /// Output format: md (default), html, or json.
    #[arg(long, value_enum)]
    pub format: Option<HandoverFormat>,
    /// Write the rendered report atomically (mode 0600) to this file path.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Open the rendered HTML report in the system browser (TTY + macOS only).
    #[arg(long)]
    pub open: bool,
}

enum OutError {
    Policy(String),
    Runtime(CmdError),
}

pub fn run(args: &HandoverArgs, ctx: &Ctx) -> CmdResult {
    if let Err(e) = validate_task_id(&args.task_id) {
        return Ok(CmdOutput::policy(
            "refused",
            format!("invalid task id: {e}"),
        ));
    }
    if (args.out.is_some() || args.open)
        && let Some(refused) = refuse_inside_agent_run(ctx, "aaf handover --out/--open")
    {
        return Ok(refused);
    }

    let format = match resolve_format(args, ctx) {
        Ok(fmt) => fmt,
        Err(policy) => return Ok(CmdOutput::policy("refused", policy)),
    };

    let Some(db) = open_db_read_only(ctx)? else {
        return Err(CmdError::new(format!(
            "task {} not found (no database at {})",
            args.task_id,
            ctx.data_dir.display()
        )));
    };
    let task = match db.get_task(&args.task_id) {
        Ok(task) => task,
        Err(PortError::NotFound(_)) => {
            return Err(CmdError::new(format!("task {} not found", args.task_id)));
        }
        Err(e) => return Err(err("task", e)),
    };
    let repo_cfg = load_repo_config(ctx, Path::new(&task.repo_root));
    let data = load_handover(&db, &ctx.data_dir, &args.task_id, &repo_cfg)
        .map_err(|e| CmdError::new(format!("handover: {e}")))?;

    let handover_json = render_json(&data);
    let rendered = match format {
        HandoverFormat::Md => render_markdown(&data),
        HandoverFormat::Html => render_html(&data, &ctx.data_dir),
        HandoverFormat::Json => {
            serde_json::to_string_pretty(&handover_json).map_err(|e| err("json", e))?
        }
    };

    let written_path = if let Some(raw_out) = &args.out {
        let resolved = if raw_out.is_absolute() {
            raw_out.clone()
        } else {
            ctx.cwd.join(raw_out)
        };
        match write_checked_output(&resolved, &ctx.data_dir, rendered.as_bytes()) {
            Ok(path) => Some(path),
            Err(OutError::Policy(why)) => return Ok(CmdOutput::policy("refused", why)),
            Err(OutError::Runtime(e)) => return Err(e),
        }
    } else if args.open {
        let default_html = ctx
            .data_dir
            .join("reports")
            .join(format!("handover-{}.html", args.task_id));
        match write_checked_output(&default_html, &ctx.data_dir, rendered.as_bytes()) {
            Ok(path) => Some(path),
            Err(OutError::Policy(why)) => return Ok(CmdOutput::policy("refused", why)),
            Err(OutError::Runtime(e)) => return Err(e),
        }
    } else {
        None
    };

    if args.open
        && let Some(ref html_path) = written_path
    {
        open_html_file(html_path)?;
    }

    let text = match &written_path {
        Some(path) => format!("wrote {} ({})", path.display(), data.status),
        None => rendered,
    };

    let mut out = CmdOutput::ok("ok", text)
        .field("task_id", json!(data.task_id))
        .field("handover_status", json!(data.status))
        .field("format", json!(format.as_str()))
        .field("handover", handover_json);
    if let Some(path) = written_path {
        out = out.field("out", json!(path.to_string_lossy().into_owned()));
    }
    Ok(out)
}

fn resolve_format(args: &HandoverArgs, ctx: &Ctx) -> Result<HandoverFormat, String> {
    if args.open {
        if !ctx.stdin_is_tty {
            return Err("aaf handover --open requires an interactive TTY".to_string());
        }
        if let Some(out) = &args.out {
            if args.format != Some(HandoverFormat::Html) {
                return Err("aaf handover --open with --out requires --format html".to_string());
            }
            if !has_html_extension(out) {
                return Err("aaf handover --open requires a .html output path".to_string());
            }
            return Ok(HandoverFormat::Html);
        }
        if let Some(fmt) = args.format
            && fmt != HandoverFormat::Html
        {
            return Err("aaf handover --open requires --format html".to_string());
        }
        return Ok(HandoverFormat::Html);
    }
    Ok(args.format.unwrap_or(HandoverFormat::Md))
}

fn has_html_extension(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| ext.eq_ignore_ascii_case("html"))
}

/// Repository config for a task's handover: the task's own repository while
/// it still exists, else the current repository's only when it is the same
/// repository (same common repo id), else the defaults with proof required
/// (fail closed: another repository's config must not waive the task's
/// proof). Shared by every channel (CLI, web UI, Telegram, cockpit) so they
/// all report the same readiness for the same task.
pub(crate) fn load_repo_config(ctx: &Ctx, task_repo: &Path) -> RepoConfig {
    if task_repo.is_dir()
        && let Ok(eff) = ctx.load_config(task_repo)
    {
        return eff.repo;
    }
    if let Ok(root) = ctx.repo_root()
        && same_repository(task_repo, &root)
        && let Ok(eff) = ctx.load_config(&root)
    {
        return eff.repo;
    }
    let mut cfg = RepoConfig::default();
    cfg.proof.required = true;
    cfg
}

/// Whether `cwd_root` belongs to the repository `task_repo` named. A gone
/// `task_repo` is identified by its path alone (git cannot read it).
fn same_repository(task_repo: &Path, cwd_root: &Path) -> bool {
    let task_id = aaf_engine::gate::stats_repo_id(task_repo);
    task_id == aaf_engine::gate::stats_repo_id(cwd_root) || task_id == aaf_fp::repo_id(cwd_root)
}

fn canonicalize_best_effort(path: &Path) -> Result<PathBuf, OutError> {
    if let Ok(canon) = fs::canonicalize(path) {
        return Ok(canon);
    }
    let mut tail: Vec<&OsStr> = Vec::new();
    let mut cur = path;
    while !cur.exists() {
        let Some(name) = cur.file_name() else {
            return Err(OutError::Policy(format!(
                "cannot resolve path {}",
                path.display()
            )));
        };
        tail.push(name);
        let Some(parent) = cur.parent() else {
            return Err(OutError::Policy(format!(
                "cannot resolve parent of {}",
                path.display()
            )));
        };
        cur = parent;
    }
    let mut base =
        fs::canonicalize(cur).map_err(|e| OutError::Runtime(err(&cur.display().to_string(), e)))?;
    for part in tail.into_iter().rev() {
        base.push(part);
    }
    Ok(base)
}

fn has_git_component(path: &Path) -> bool {
    path.components().any(|c| match c {
        Component::Normal(seg) => seg.to_str().is_some_and(|s| s.eq_ignore_ascii_case(".git")),
        _ => false,
    })
}

fn is_protected_output_path(path: &Path) -> bool {
    for comp in path.components() {
        if let Component::Normal(seg) = comp
            && let Some(s) = seg.to_str()
            && (s.eq_ignore_ascii_case(".claude") || s.eq_ignore_ascii_case(".agents"))
        {
            return true;
        }
    }
    let Some(name) = path.file_name().and_then(OsStr::to_str) else {
        return true;
    };
    let lower = name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "claude.md"
            | ".mcp.json"
            | "agents.md"
            | "google-services.json"
            | "googleservice-info.plist"
    ) || lower.starts_with(".env")
    {
        return true;
    }
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| ext.eq_ignore_ascii_case("keystore") || ext.eq_ignore_ascii_case("jks"))
}

fn check_existing_not_symlink(path: &Path) -> Result<(), OutError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(OutError::Policy(format!(
            "refusing to write through symlink {}",
            path.display()
        ))),
        Ok(meta) if !meta.is_file() => Err(OutError::Policy(format!(
            "refusing to overwrite non-regular file {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(OutError::Runtime(err(&path.display().to_string(), e))),
    }
}

fn write_checked_output(
    raw_target: &Path,
    data_dir: &Path,
    bytes: &[u8],
) -> Result<PathBuf, OutError> {
    if has_git_component(raw_target) {
        return Err(OutError::Policy(format!(
            "refusing to write inside .git directory: {}",
            raw_target.display()
        )));
    }
    let Some(file_name) = raw_target.file_name() else {
        return Err(OutError::Policy(format!(
            "invalid output file path: {}",
            raw_target.display()
        )));
    };
    let Some(parent) = raw_target.parent() else {
        return Err(OutError::Policy(format!(
            "output path has no parent directory: {}",
            raw_target.display()
        )));
    };
    check_existing_not_symlink(raw_target)?;

    let canon_data = canonicalize_best_effort(data_dir)?;
    let allowed_reports = canon_data.join("reports");

    // Allow auto-creating `<data_dir>/reports` (and safe subdirs under it) if missing.
    if !parent.exists() {
        let candidate_parent = canonicalize_best_effort(parent)?;
        let inside_reports =
            candidate_parent == allowed_reports || candidate_parent.starts_with(&allowed_reports);
        if inside_reports && !has_git_component(&candidate_parent) {
            let mut ancestor = parent;
            while !ancestor.exists() {
                let Some(next) = ancestor.parent() else { break };
                ancestor = next;
            }
            if let Ok(meta) = fs::symlink_metadata(ancestor)
                && meta.file_type().is_symlink()
            {
                return Err(OutError::Policy(format!(
                    "refusing to create directory through symlink {}",
                    ancestor.display()
                )));
            }
            private::create_dir(parent)
                .map_err(|e| OutError::Runtime(err(&parent.display().to_string(), e)))?;
        } else {
            return Err(OutError::Policy(format!(
                "output parent directory does not exist: {}",
                parent.display()
            )));
        }
    }

    let canon_parent = fs::canonicalize(parent)
        .map_err(|e| OutError::Runtime(err(&parent.display().to_string(), e)))?;
    let canon_target = canon_parent.join(file_name);

    if has_git_component(&canon_target) {
        return Err(OutError::Policy(format!(
            "refusing to write inside .git directory: {}",
            canon_target.display()
        )));
    }
    if canon_target.starts_with(&canon_data)
        && (!canon_target.starts_with(&allowed_reports) || canon_target == allowed_reports)
    {
        return Err(OutError::Policy(format!(
            "refusing to write inside AAF_DATA_DIR outside reports/: {}",
            canon_target.display()
        )));
    }
    if is_protected_output_path(&canon_target) {
        return Err(OutError::Policy(format!(
            "refusing to write to protected path: {}",
            canon_target.display()
        )));
    }
    check_existing_not_symlink(&canon_target)?;
    write_atomic_0600(&canon_parent, &canon_target, bytes)?;
    Ok(canon_target)
}

fn write_atomic_0600(parent: &Path, target: &Path, bytes: &[u8]) -> Result<(), OutError> {
    let seq = TEMP_OUT_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".aaf-handover-tmp-{}-{seq}", std::process::id()));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&temp)
        .map_err(|e| OutError::Runtime(err(&temp.display().to_string(), e)))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
            drop(file);
            let _ = fs::remove_file(&temp);
            return Err(OutError::Runtime(err(&temp.display().to_string(), e)));
        }
    }
    if let Err(e) = file.write_all(bytes) {
        drop(file);
        let _ = fs::remove_file(&temp);
        return Err(OutError::Runtime(err(&temp.display().to_string(), e)));
    }
    drop(file);
    if let Err(policy) = check_existing_not_symlink(target) {
        let _ = fs::remove_file(&temp);
        return Err(policy);
    }
    if let Err(e) = fs::rename(&temp, target) {
        let _ = fs::remove_file(&temp);
        return Err(OutError::Runtime(err(&target.display().to_string(), e)));
    }
    Ok(())
}

fn open_html_file(path: &Path) -> Result<(), CmdError> {
    let lmeta = fs::symlink_metadata(path).map_err(|e| err(&path.display().to_string(), e))?;
    if lmeta.file_type().is_symlink() || !lmeta.is_file() {
        return Err(CmdError::new(format!(
            "refusing to open non-regular file {}",
            path.display()
        )));
    }
    let canon = fs::canonicalize(path).map_err(|e| err(&path.display().to_string(), e))?;
    if !has_html_extension(&canon) {
        return Err(CmdError::new(format!(
            "refusing to open non-HTML file {}",
            canon.display()
        )));
    }
    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("/usr/bin/open")
            .arg("--")
            .arg(&canon)
            .status()
            .map_err(|e| err("/usr/bin/open", e))?;
        if !status.success() {
            return Err(CmdError::new(format!(
                "/usr/bin/open exited with status {status}"
            )));
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = canon;
        Err(CmdError::new(
            "aaf handover --open is only supported on macOS (/usr/bin/open)",
        ))
    }
}
