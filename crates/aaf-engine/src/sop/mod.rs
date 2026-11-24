//! Declarative SOP workflow engine, built-in blueprints, discovery, and resumable runner.

pub mod builtin;
pub mod runner;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use aaf_domain::gate::GateMode;
use aaf_domain::sop::{SopSpec, StageKind, is_valid_stage_id};

use self::builtin::{builtin_sops, find_builtin, render_toml};
use self::runner::SopError;

/// Relative directory inside a repository where custom SOP TOML files live.
pub const REPO_SOPS_REL: &str = ".autofactory/sops";

const MAX_SOP_TOML_BYTES: u64 = 512 * 1024;
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Where an SOP specification was loaded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SopSource {
    Builtin,
    Repo(PathBuf),
}

impl SopSource {
    pub fn label(&self) -> String {
        match self {
            SopSource::Builtin => "builtin".to_string(),
            SopSource::Repo(path) => path.display().to_string(),
        }
    }
}

/// An SOP specification together with its origin (repo file or built-in blueprint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSop {
    pub spec: SopSpec,
    pub source: SopSource,
}

/// Loads, parses, and validates a single SOP TOML file (refusing symlinks).
pub fn load_sop_file(path: &Path) -> Result<SopSpec, SopError> {
    reject_parent_dir(path)?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        if is_repo_sops_dir(parent) {
            if !check_existing_real_dir(parent)? {
                return Err(SopError::NotFound(format!("SOP file {}", path.display())));
            }
        } else {
            match fs::metadata(parent) {
                Ok(m) if m.is_dir() => {}
                Ok(_) => {
                    return Err(SopError::InvalidPath(format!(
                        "{} is not a directory",
                        parent.display()
                    )));
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return Err(SopError::NotFound(format!("SOP file {}", path.display())));
                }
                Err(e) => return Err(SopError::Io(format!("stat {}: {e}", parent.display()))),
            }
        }
    }
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(SopError::NotFound(format!("SOP file {}", path.display())));
        }
        Err(e) => return Err(SopError::Io(format!("stat {}: {e}", path.display()))),
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(SopError::InvalidPath(format!(
            "{} is a symlink or not a regular file",
            path.display()
        )));
    }
    let file =
        fs::File::open(path).map_err(|e| SopError::Io(format!("open {}: {e}", path.display())))?;
    let mut text = String::new();
    file.take(MAX_SOP_TOML_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| SopError::Io(format!("read {}: {e}", path.display())))?;
    if u64::try_from(text.len()).unwrap_or(u64::MAX) > MAX_SOP_TOML_BYTES {
        return Err(SopError::InvalidPath(format!(
            "SOP file {} exceeds {} bytes",
            path.display(),
            MAX_SOP_TOML_BYTES
        )));
    }
    let spec: SopSpec = toml::from_str(&text)
        .map_err(|e| SopError::Parse(format!("parse {}: {e}", path.display())))?;
    spec.validate()?;
    Ok(spec)
}

/// Discovers and validates all `*.toml` SOP files under `<repo_root>/.autofactory/sops/`.
pub fn load_repo_sops(repo_root: &Path) -> Result<Vec<DiscoveredSop>, SopError> {
    let dir = repo_root.join(REPO_SOPS_REL);
    if !check_existing_real_dir(&dir)? {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    let entries =
        fs::read_dir(&dir).map_err(|e| SopError::Io(format!("read {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| SopError::Io(format!("dir entry: {e}")))?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "toml") {
            paths.push(path);
        }
    }
    paths.sort();

    let mut seen_names = HashSet::with_capacity(paths.len());
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let spec = load_sop_file(&path)?;
        if !seen_names.insert(spec.name.clone()) {
            return Err(SopError::InvalidInput(format!(
                "duplicate SOP {:?} declared in {}",
                spec.name,
                path.display()
            )));
        }
        out.push(DiscoveredSop {
            spec,
            source: SopSource::Repo(path),
        });
    }
    Ok(out)
}

/// Lists all available SOPs: repo SOPs take precedence over built-in blueprints of the same name.
pub fn list_all_sops(repo_root: Option<&Path>) -> Result<Vec<DiscoveredSop>, SopError> {
    let mut result = match repo_root {
        Some(root) => load_repo_sops(root)?,
        None => Vec::new(),
    };
    let seen: HashSet<String> = result.iter().map(|d| d.spec.name.clone()).collect();
    for spec in builtin_sops() {
        if !seen.contains(&spec.name) {
            result.push(DiscoveredSop {
                spec,
                source: SopSource::Builtin,
            });
        }
    }
    Ok(result)
}

/// Resolves an SOP by name from `<repo_root>/.autofactory/sops/` first, then built-in blueprints.
pub fn resolve_sop(repo_root: Option<&Path>, name: &str) -> Result<DiscoveredSop, SopError> {
    if name.trim().is_empty() {
        return Err(SopError::InvalidInput(
            "SOP name must not be empty".to_string(),
        ));
    }
    if let Some(root) = repo_root {
        for discovered in load_repo_sops(root)? {
            if discovered.spec.name == name {
                return Ok(discovered);
            }
        }
    }
    if let Some(spec) = find_builtin(name) {
        spec.validate()?;
        return Ok(DiscoveredSop {
            spec,
            source: SopSource::Builtin,
        });
    }
    Err(SopError::NotFound(format!("SOP {name:?}")))
}

/// Renders a deterministic ASCII DAG diagram of an [`SopSpec`] in topological order.
pub fn render_ascii_dag(spec: &SopSpec) -> Result<String, SopError> {
    let order = spec.validate()?;
    let mut lines = Vec::with_capacity(order.len() * 2 + 2);
    lines.push(format!(
        "SOP: {} (v{}, profile={}, max_reworks={})",
        spec.name, spec.version, spec.profile, spec.max_reworks
    ));
    if !spec.description.is_empty() {
        lines.push(format!("  {}", spec.description));
    }
    for (idx, id) in order.iter().enumerate() {
        let Some(stage) = spec.stage(id) else {
            continue;
        };
        let deps = if stage.depends_on.is_empty() {
            String::new()
        } else {
            format!(" <- [{}]", stage.depends_on.join(", "))
        };
        let rework = match &stage.on_fail_goto {
            Some(target) => format!("  [on_fail -> {target}]"),
            None => String::new(),
        };
        lines.push(format!(
            "  [{}] {} ({}) — {}{}{}",
            idx + 1,
            stage.id,
            format_kind_tag(&stage.kind),
            stage.title,
            deps,
            rework
        ));
        if idx + 1 < order.len() {
            lines.push("      |".to_string());
            lines.push("      v".to_string());
        }
    }
    Ok(lines.join("\n"))
}

fn format_kind_tag(kind: &StageKind) -> String {
    match kind {
        StageKind::Agent {
            role, read_only, ..
        } => {
            if *read_only {
                format!("agent:{role}, ro")
            } else {
                format!("agent:{role}")
            }
        }
        StageKind::Script { heavy, .. } => {
            if *heavy {
                "script:heavy".to_string()
            } else {
                "script".to_string()
            }
        }
        StageKind::Gate { mode } => {
            let m = match mode {
                GateMode::Scoped => "scoped",
                GateMode::Full => "full",
                GateMode::Release => "release",
            };
            format!("gate:{m}")
        }
        StageKind::HumanGate { .. } => "human_gate".to_string(),
    }
}

/// Scaffolds a built-in SOP blueprint into `<repo_root>/.autofactory/sops/<name>.toml`,
/// refusing symlinks, invalid names, and overwriting existing files unless `force` is true.
pub fn init_sop_file(
    repo_root: &Path,
    blueprint: &str,
    name: Option<&str>,
    force: bool,
) -> Result<PathBuf, SopError> {
    let mut spec = find_builtin(blueprint).ok_or_else(|| {
        let available: Vec<String> = builtin_sops().into_iter().map(|s| s.name).collect();
        SopError::NotFound(format!(
            "blueprint {blueprint:?} (available: {})",
            available.join(", ")
        ))
    })?;
    if let Some(custom) = name {
        if !is_valid_stage_id(custom) {
            return Err(SopError::InvalidInput(format!(
                "SOP name {custom:?} must match [a-z0-9_-]+"
            )));
        }
        spec.name = custom.to_string();
    }
    spec.validate()?;

    let sops_dir = repo_root.join(REPO_SOPS_REL);
    ensure_real_dir_mode(&sops_dir, None)?;

    let dest = sops_dir.join(format!("{}.toml", spec.name));
    refuse_symlink_or_non_file(&dest)?;
    if dest.exists() && !force {
        return Err(SopError::AlreadyExists(dest));
    }

    let toml_text = render_toml(&spec).map_err(SopError::Parse)?;
    atomic_write_bytes(&sops_dir, &dest, toml_text.as_bytes(), None)?;
    Ok(dest)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), SopError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| SopError::Io(format!("chmod {:o} {}: {e}", mode, path.display())))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), SopError> {
    Ok(())
}

fn reject_parent_dir(path: &Path) -> Result<(), SopError> {
    if path.as_os_str().is_empty() {
        return Err(SopError::InvalidPath("directory path is empty".to_string()));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(SopError::InvalidPath(format!(
            "path {} must not contain `..`",
            path.display()
        )));
    }
    Ok(())
}

fn is_repo_sops_dir(dir: &Path) -> bool {
    dir.file_name().is_some_and(|n| n == "sops")
        && dir
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|n| n == ".autofactory")
}

/// Splits `dir` into `(base_ancestor, managed_tail)` so OS/user symlink ancestors
/// above `repo_root` or `data_dir` are allowed, while managed subpath components
/// (`.autofactory`, `.autofactory/sops`, `sops`) are strictly checked via `symlink_metadata`.
fn split_managed_dir(dir: &Path) -> (PathBuf, Vec<std::ffi::OsString>) {
    let Some(leaf) = dir.file_name() else {
        return (dir.to_path_buf(), Vec::new());
    };
    let Some(parent) = dir.parent() else {
        return (PathBuf::new(), vec![leaf.to_os_string()]);
    };
    if is_repo_sops_dir(dir)
        && let Some(grandparent) = parent.parent()
    {
        return (
            grandparent.to_path_buf(),
            vec![
                std::ffi::OsString::from(".autofactory"),
                std::ffi::OsString::from("sops"),
            ],
        );
    }
    (parent.to_path_buf(), vec![leaf.to_os_string()])
}

/// Verifies `dir` has no `..` and its managed subpath components are real non-symlink directories.
/// Returns `Ok(true)` if `dir` exists as a real directory, `Ok(false)` if any component is missing.
pub(crate) fn check_existing_real_dir(dir: &Path) -> Result<bool, SopError> {
    reject_parent_dir(dir)?;
    let (base, managed) = split_managed_dir(dir);
    if !base.as_os_str().is_empty() {
        match fs::metadata(&base) {
            Ok(meta) if !meta.is_dir() => {
                return Err(SopError::InvalidPath(format!(
                    "{} is not a directory",
                    base.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(SopError::Io(format!("stat {}: {e}", base.display()))),
        }
    }
    let mut cur = base;
    for part in managed {
        cur.push(part);
        match fs::symlink_metadata(&cur) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(SopError::InvalidPath(format!(
                    "{} is a symlink",
                    cur.display()
                )));
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(SopError::InvalidPath(format!(
                    "{} is not a directory",
                    cur.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(SopError::Io(format!("stat {}: {e}", cur.display()))),
        }
    }
    Ok(true)
}

/// Ensures `dir` exists as a real non-symlink managed directory, optionally applying POSIX `mode`.
pub(crate) fn ensure_real_dir_mode(dir: &Path, mode: Option<u32>) -> Result<(), SopError> {
    reject_parent_dir(dir)?;
    let (base, managed) = split_managed_dir(dir);
    if !base.as_os_str().is_empty() {
        match fs::metadata(&base) {
            Ok(meta) if !meta.is_dir() => {
                return Err(SopError::InvalidPath(format!(
                    "{} is not a directory",
                    base.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(&base)
                    .map_err(|err| SopError::Io(format!("create {}: {err}", base.display())))?;
            }
            Err(e) => return Err(SopError::Io(format!("stat {}: {e}", base.display()))),
        }
    }
    let mut cur = base;
    let last_idx = managed.len().saturating_sub(1);
    for (idx, part) in managed.into_iter().enumerate() {
        cur.push(part);
        match fs::symlink_metadata(&cur) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(SopError::InvalidPath(format!(
                    "{} is a symlink",
                    cur.display()
                )));
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(SopError::InvalidPath(format!(
                    "{} is not a directory",
                    cur.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                #[cfg(unix)]
                if let Some(m) = mode
                    && idx == last_idx
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(m);
                }
                match builder.create(&cur) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(err) => {
                        return Err(SopError::Io(format!("create {}: {err}", cur.display())));
                    }
                }
                let meta = fs::symlink_metadata(&cur)
                    .map_err(|err| SopError::Io(format!("stat {}: {err}", cur.display())))?;
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    return Err(SopError::InvalidPath(format!(
                        "{} is a symlink or not a directory",
                        cur.display()
                    )));
                }
            }
            Err(e) => return Err(SopError::Io(format!("stat {}: {e}", cur.display()))),
        }
    }
    if let Some(m) = mode {
        set_mode(dir, m)?;
    }
    Ok(())
}

/// Refuses if `path` exists and is a symlink or non-regular file.
pub(crate) fn refuse_symlink_or_non_file(path: &Path) -> Result<(), SopError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => Err(SopError::InvalidPath(
            format!("{} is a symlink or not a regular file", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SopError::Io(format!("stat {}: {e}", path.display()))),
    }
}

/// Atomically writes `bytes` to `dest` via a temp file in `dir`, tightening mode when specified.
pub(crate) fn atomic_write_bytes(
    dir: &Path,
    dest: &Path,
    bytes: &[u8],
    mode: Option<u32>,
) -> Result<(), SopError> {
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".sop.aaf-tmp-{}-{seq}", std::process::id()));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(m) = mode {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(m);
    }
    let mut file = opts
        .open(&tmp)
        .map_err(|e| SopError::Io(format!("create temp {}: {e}", tmp.display())))?;
    if let Err(e) = file.write_all(bytes).and_then(|()| file.flush()) {
        drop(file);
        let _ = fs::remove_file(&tmp);
        return Err(SopError::Io(format!("write {}: {e}", tmp.display())));
    }
    drop(file);
    if let Some(m) = mode
        && let Err(e) = set_mode(&tmp, m)
    {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, dest) {
        let _ = fs::remove_file(&tmp);
        return Err(SopError::Io(format!(
            "rename {} -> {}: {e}",
            tmp.display(),
            dest.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod regression_tests;
