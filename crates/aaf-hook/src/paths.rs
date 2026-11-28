//! Path helpers: lexical resolution, containment and protected-name patterns.

use std::path::{Component, Path, PathBuf};

mod resolving;

pub(crate) use resolving::Resolving;
use resolving::{canonical, known_missing};

fn expand_unnormalized(raw: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    // `$HOMEWORK` is not `$HOME`: the prefix must end at a separator.
    let home_rest = ["~", "$HOME", "${HOME}"]
        .iter()
        .find_map(|prefix| raw.strip_prefix(prefix))
        .filter(|rest| rest.is_empty() || rest.starts_with('/'));
    let pwd_rest = ["~+", "$PWD", "${PWD}"]
        .iter()
        .find_map(|prefix| raw.strip_prefix(prefix))
        .filter(|rest| rest.is_empty() || rest.starts_with('/'));
    let tmp_rest = ["$TMPDIR", "${TMPDIR}"]
        .iter()
        .find_map(|prefix| raw.strip_prefix(prefix))
        .filter(|rest| rest.is_empty() || rest.starts_with('/'));
    let path = if let (Some(rest), Some(home)) = (home_rest, home) {
        home.join(rest.trim_start_matches('/'))
    } else if let Some(rest) = pwd_rest {
        cwd.join(rest.trim_start_matches('/'))
    } else if let Some(rest) = tmp_rest
        && let Some(tmpdir) = std::env::var_os("TMPDIR").filter(|v| !v.is_empty())
    {
        PathBuf::from(tmpdir).join(rest.trim_start_matches('/'))
    } else {
        PathBuf::from(raw)
    };
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

/// Resolves `raw` the way the shell would for a literal word: `~`, `$HOME`,
/// `$PWD`, `$TMPDIR` and relative paths, then `.`/`..` without touching the file system.
pub fn expand(raw: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    normalize(&expand_unnormalized(raw, cwd, home))
}

/// Like [`expand`], but when a `..` component is present, resolves symlinks in
/// the preceding path left-to-right BEFORE applying `..` (matching OS kernel
/// lookup semantics for `symlink/../target`).
pub fn expand_real_dotdot(raw: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    let unnorm = expand_unnormalized(raw, cwd, home);
    if !unnorm
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return normalize(&unnorm);
    }
    let mut cur = PathBuf::new();
    for component in unnorm.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !cur.as_os_str().is_empty()
                    && !matches!(cur.components().next_back(), Some(Component::ParentDir))
                {
                    cur = real(&cur);
                }
                match cur.components().next_back() {
                    None | Some(Component::ParentDir) => cur.push(".."),
                    Some(_) => {
                        cur.pop();
                    }
                }
            }
            other => cur.push(other.as_os_str()),
        }
    }
    cur
}

/// True when `raw` starts with `~`, `$HOME`, or `${HOME}` (whole path or prefix).
pub fn references_home(raw: &str) -> bool {
    ["~", "$HOME", "${HOME}"].iter().any(|prefix| {
        raw.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

/// True when `raw` still contains an unresolved shell variable (`$VAR`, `${...}`,
/// `$(...)`) or backtick command substitution after stripping known safe prefixes
/// (`$HOME`, `${HOME}`, `$PWD`, `${PWD}`, `$TMPDIR`, `${TMPDIR}`, `~`, `~+`), or
/// starts with a tilde prefix the hook cannot resolve (`~-`, `~1`, `~user`).
pub fn has_unresolved_var(raw: &str) -> bool {
    let known = [
        "${HOME}",
        "$HOME",
        "${PWD}",
        "$PWD",
        "${TMPDIR}",
        "$TMPDIR",
        "~+",
        "~",
    ]
    .iter()
    .find_map(|prefix| {
        raw.strip_prefix(prefix)
            .filter(|rest| rest.is_empty() || rest.starts_with('/'))
    });
    if known.is_none() && raw.starts_with('~') {
        return true;
    }
    let tail = known.unwrap_or(raw);
    tail.contains('$') || tail.contains('`')
}

/// System roots whose recursive deletion is always catastrophic.
const SYSTEM_EXACT_OR_PREFIX: &[&str] = &[
    "/System",
    "/Library",
    "/usr",
    "/bin",
    "/sbin",
    "/etc",
    "/Applications",
    "/opt",
];

/// Container roots whose direct deletion is catastrophic (while specific
/// repo-local subpaths inside `/Volumes/<vol>/...` or `/Users/<u >/repo/...`
/// are governed separately by repo-root containment).
const CONTAINER_EXACT: &[&str] = &[
    "/",
    "/Users",
    "/Volumes",
    "/var",
    "/private",
    "/private/var",
    "/tmp",
    "/private/tmp",
];

fn check_one_system_or_home(p: &Path, home: Option<&Path>) -> Option<&'static str> {
    if p == Path::new("/") {
        return Some("filesystem root `/`");
    }
    if let Some(h) = home {
        let h_norm = normalize(h);
        if p == h_norm || h_norm.starts_with(p) {
            return Some("user home directory");
        }
    }
    let s = p.to_string_lossy();
    if CONTAINER_EXACT.iter().any(|root| s == *root) {
        return Some("system root directory");
    }
    // Block top-level mounted volume root `/Volumes/<vol>` itself.
    if let Some(rest) = s.strip_prefix("/Volumes/")
        && !rest.is_empty()
        && !rest.contains('/')
    {
        return Some("mounted volume root");
    }
    if SYSTEM_EXACT_OR_PREFIX
        .iter()
        .any(|sys| p == Path::new(sys) || p.starts_with(sys))
    {
        return Some("system directory");
    }
    None
}

/// Returns a reason if `path` (or its symlink target) is `/` or a home/system root.
pub fn is_system_or_home_danger(path: &Path, home: Option<&Path>) -> Option<&'static str> {
    let real_home = home.map(real);
    check_one_system_or_home(path, home)
        .or_else(|| check_one_system_or_home(&real(path), real_home.as_deref()))
}

fn strictly_inside(child: &Path, parent: &Path) -> bool {
    child.starts_with(parent) && child != parent
}

/// True iff `path` (and its symlink target) is strictly inside `/tmp`,
/// `/private/tmp`, or `$TMPDIR` (never the root itself, and never escaping via
/// symlinks). When `repo_root` itself lives inside `/tmp` or `$TMPDIR` (e.g.
/// tests or temp workspaces), relative escapes and siblings inside the same
/// temp container directory are not exempted.
pub fn is_allowed_tmp_subpath(raw: &str, path: &Path, repo_root: Option<&Path>) -> bool {
    let resolved = real(path);
    let in_tmp = |p: &Path| {
        strictly_inside(p, Path::new("/tmp")) || strictly_inside(p, Path::new("/private/tmp"))
    };
    if in_tmp(path) && in_tmp(&resolved) {
        let repo_in_tmp = repo_root.is_some_and(|r| in_tmp(r) || in_tmp(&real(r)));
        let explicit_tmp = raw.starts_with("/tmp/") || raw.starts_with("/private/tmp/");
        let in_repo_parent_container = repo_root
            .and_then(Path::parent)
            .filter(|parent| in_tmp(parent) || in_tmp(&real(parent)))
            .is_some_and(|parent| is_inside(path, parent) || is_inside(&resolved, parent));
        if !repo_in_tmp || (explicit_tmp && !in_repo_parent_container) {
            return true;
        }
    }
    if let Some(tmpdir_os) = std::env::var_os("TMPDIR").filter(|v| !v.is_empty()) {
        let tmpdir = normalize(&PathBuf::from(tmpdir_os));
        let tmpdir_real = real(&tmpdir);
        let explicit_tmpdir = raw.starts_with("$TMPDIR/") || raw.starts_with("${TMPDIR}/");
        let repo_in_tmpdir = repo_root.is_some_and(|r| {
            strictly_inside(r, &tmpdir) || strictly_inside(&real(r), &tmpdir_real)
        });
        if (explicit_tmpdir || !repo_in_tmpdir)
            && (strictly_inside(path, &tmpdir) || strictly_inside(path, &tmpdir_real))
            && (strictly_inside(&resolved, &tmpdir) || strictly_inside(&resolved, &tmpdir_real))
        {
            return true;
        }
    }
    false
}

const SECRET_SUFFIXES: &[&str] = &[".keystore", ".jks", ".p12", ".pem", ".mobileprovision"];
const SECRET_EXACT: &[&str] = &[
    "keystore.properties",
    "google-services.json",
    "googleservice-info.plist",
];
const SECRET_ENV_ALLOWLIST: &[&str] = &[".env.example", ".env.sample", ".env.template"];

/// Matches files that hold secrets or credentials (`.env`, `.env.*` except
/// public templates, keystores, `keystore.properties`, `google-services.json`,
/// PEM/P12 keys, SSH private keys).
pub fn is_secret_file(path: &Path) -> Option<&'static str> {
    let name = fold(path.file_name()?.to_str()?);
    if SECRET_ENV_ALLOWLIST.contains(&name.as_str()) {
        return None;
    }
    if name == ".env" {
        return Some(".env");
    }
    if name.starts_with(".env.") {
        return Some(".env.*");
    }
    if let Some(exact) = SECRET_EXACT.iter().find(|n| name == **n) {
        return Some(exact);
    }
    if name == "id_rsa"
        || name.starts_with("id_rsa.")
        || name == "id_ed25519"
        || name.starts_with("id_ed25519.")
    {
        return Some("ssh private key");
    }
    SECRET_SUFFIXES
        .iter()
        .find(|s| name.ends_with(**s))
        .copied()
}

/// Removes `.` and resolves `..` lexically. A `..` that climbs above a
/// relative path's start is kept: where it leads is unknown, and dropping it
/// would turn `../data/aaf.db` into the harmless-looking `data/aaf.db`.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                None | Some(Component::ParentDir) => out.push(".."),
                // Nothing is above the root, where `pop` does nothing.
                Some(_) => {
                    out.pop();
                }
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A normalised path that leaves the unknown directory it is relative to.
pub fn escapes(path: &Path) -> bool {
    path.components().next() == Some(Component::ParentDir)
}

/// Symlink-free form of `path`: the deepest existing ancestor is resolved and
/// the missing tail is kept, so a file that does not exist yet still compares.
pub fn real(path: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut current = path;
    loop {
        if !current.parent().is_some_and(known_missing)
            && let Some(resolved) = canonical(current)
        {
            let mut out = resolved;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name);
                current = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// True when `path` is `dir` or below it, lexically or through symlinks.
pub fn is_inside(path: &Path, dir: &Path) -> bool {
    path.starts_with(dir) || real(path).starts_with(real(dir))
}

const SUFFIXES: &[&str] = &[".keystore", ".jks", ".p12", ".mobileprovision"];
const NAMES: &[&str] = &["google-services.json", "googleservice-info.plist"];
const NESTED: &[(&str, &str)] = &[
    (".autofactory", "config.toml"),
    (".autofactory", "memory.md"),
    (".claude", "settings.json"),
    (".claude", "settings.local.json"),
];

/// Names a glob could expand to, one per protected pattern.
pub const SAMPLE_NAMES: &[&str] = &[
    ".env",
    ".env.local",
    "a.keystore",
    "a.jks",
    "a.p12",
    "a.mobileprovision",
    "google-services.json",
    "GoogleService-Info.plist",
];

/// `text` lowercased as APFS, the default macOS file system, compares names:
/// ASCII case ignored, and each character whose Unicode case folding is
/// ASCII replaced by it (`ſ`, the Kelvin sign, `ß`, the `ﬀ`…`ﬆ` ligatures), so
/// `.claude/ſettings.json` opens `.claude/settings.json` there.
pub fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let folded = match c {
            '\u{17F}' => "s",
            '\u{212A}' => "k",
            '\u{DF}' | '\u{1E9E}' => "ss",
            '\u{FB00}' => "ff",
            '\u{FB01}' => "fi",
            '\u{FB02}' => "fl",
            '\u{FB03}' => "ffi",
            '\u{FB04}' => "ffl",
            '\u{FB05}' | '\u{FB06}' => "st",
            c => {
                out.push(c.to_ascii_lowercase());
                continue;
            }
        };
        out.push_str(folded);
    }
    out
}

/// The protected pattern `path` matches, if any. Folded like the default
/// macOS file system compares names. Public `.env` templates
/// (`.env.example`, `.env.sample`, `.env.template`) are exempted.
pub fn protected_name(path: &Path) -> Option<&'static str> {
    let name = fold(path.file_name()?.to_str()?);
    if SECRET_ENV_ALLOWLIST.contains(&name.as_str()) {
        return None;
    }
    if name == ".env" {
        return Some(".env");
    }
    if name.starts_with(".env.") {
        return Some(".env.*");
    }
    if let Some(suffix) = SUFFIXES.iter().find(|s| name.ends_with(*s)) {
        return Some(suffix);
    }
    if let Some(exact) = NAMES.iter().find(|n| name == **n) {
        return Some(exact);
    }
    let parent = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|p| p.to_str())
        .map(fold)?;
    if let Some(hit) = NESTED
        .iter()
        .find(|(dir, file)| parent == *dir && name == *file)
        .map(|(_, file)| *file)
    {
        return Some(hit);
    }
    // Protect any file under `.autofactory/roles/**` (APFS case-folded).
    let mut prev_autofactory = false;
    for comp in path.parent()?.components() {
        if let Component::Normal(seg) = comp
            && let Some(s) = seg.to_str()
        {
            let folded = fold(s);
            if prev_autofactory && folded == "roles" {
                return Some(".autofactory/roles/*");
            }
            prev_autofactory = folded == ".autofactory";
        } else {
            prev_autofactory = false;
        }
    }
    None
}

pub fn has_glob(text: &str) -> bool {
    text.contains(['*', '?', '['])
}

/// True when `last` is a glob with a bounded tail (does not end with `*`),
/// e.g. `release.keystor?` or `*.pe?`, unlike open-ended `README*`/`mod.*`.
pub fn bounded_glob(last: &str) -> bool {
    has_glob(last) && !last.ends_with('*')
}

#[derive(Clone, Copy, PartialEq)]
enum Part {
    Star,
    Any,
    Lit(char),
}

fn glob_parts(pattern: &str) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '*' => parts.push(Part::Star),
            '?' => parts.push(Part::Any),
            '[' => {
                let _ = chars.by_ref().find(|&inner| inner == ']');
                parts.push(Part::Any);
            }
            other => parts.push(Part::Lit(other.to_ascii_lowercase())),
        }
    }
    parts
}

/// Some name matches both globs. A table over the two part lists, so a
/// hostile pattern costs memory proportional to its length, never stack.
fn globs_intersect(a: &[Part], b: &[Part]) -> bool {
    let (rows, cols) = (a.len(), b.len());
    // `table[i][j]`: the tails `a[i..]` and `b[j..]` share a name.
    let mut table = vec![vec![false; cols + 1]; rows + 1];
    for i in (0..=rows).rev() {
        for j in (0..=cols).rev() {
            table[i][j] = match (a.get(i), b.get(j)) {
                (None, None) => true,
                (Some(Part::Star), _) => table[i + 1][j] || (j < cols && table[i][j + 1]),
                (_, Some(Part::Star)) => table[i][j + 1] || (i < rows && table[i + 1][j]),
                (Some(x), Some(y)) => {
                    (*x == Part::Any || *y == Part::Any || x == y) && table[i + 1][j + 1]
                }
                _ => false,
            };
        }
    }
    table[0][0]
}

/// Longest `find -name` pattern still analysed; longer ones are refused.
const MAX_PATTERN: usize = 256;
/// The protected patterns as globs over one file name. The nested files
/// (`.claude/settings.json`, …) are listed by name alone: wider, never narrower.
const PROTECTED_GLOBS: &[&str] = &[
    ".env",
    "*.keystore",
    "*.jks",
    "*.p12",
    "*.mobileprovision",
    "google-services.json",
    "googleservice-info.plist",
    "config.toml",
    "settings.json",
    "settings.local.json",
];
const ENV_VARIANTS: &[&str] = &[
    ".env.local",
    ".env.production",
    ".env.development",
    ".env.staging",
    ".env.test",
];

fn may_name_protected_with(pattern: &str, suffix: &str, include_nested: bool) -> bool {
    if pattern.len() > MAX_PATTERN || suffix.len() > MAX_PATTERN {
        return true;
    }
    let folded = fold(pattern);
    if SECRET_ENV_ALLOWLIST.contains(&folded.strip_suffix(&fold(suffix)).unwrap_or("")) {
        return false;
    }
    let parts = glob_parts(pattern);
    let sfx: Vec<_> = suffix
        .chars()
        .map(|c| Part::Lit(c.to_ascii_lowercase()))
        .collect();
    let hits = |g: &str| globs_intersect(&parts, &[glob_parts(g), sfx.clone()].concat());
    PROTECTED_GLOBS
        .iter()
        .filter(|g| include_nested || !NESTED.iter().any(|(_, f)| f == *g))
        .any(|g| hits(g))
        || (hits(".env.*")
            && (parts.first() != Some(&Part::Star)
                || ENV_VARIANTS
                    .iter()
                    .any(|n| glob_match(pattern, &format!("{n}{suffix}")))))
}

/// A file-name glob (as given to `find -name`) can match a protected file.
pub fn may_name_protected(pattern: &str) -> bool {
    may_name_protected_with(pattern, "", true)
}

/// Like [`may_name_protected`], when a decompressor strips `suffix` from matched names.
pub fn may_name_protected_with_suffix(pattern: &str, suffix: &str) -> bool {
    may_name_protected_with(pattern, suffix, true)
}

/// Like [`may_name_protected`], excluding names only protected inside `.claude`/`.autofactory`.
pub fn may_name_protected_anywhere(pattern: &str) -> bool {
    may_name_protected_with(pattern, "", false)
}

/// A file-name glob can match a secret file (returns the matched pattern).
pub fn may_name_secret(pattern: &str) -> Option<&'static str> {
    if pattern.len() > MAX_PATTERN {
        return Some("secret file");
    }
    if SECRET_ENV_ALLOWLIST.contains(&fold(pattern).as_str()) {
        return None;
    }
    let parts = glob_parts(pattern);
    let hits = |glob: &str| globs_intersect(&parts, &glob_parts(glob));
    if let Some(&pat) = [".env", "id_rsa", "id_ed25519"]
        .iter()
        .chain(SECRET_EXACT)
        .find(|g| hits(g))
    {
        return Some(pat);
    }
    if let Some(&suf) = SECRET_SUFFIXES.iter().find(|s| hits(&format!("*{s}"))) {
        return Some(suf);
    }
    let prefix_pat = [".env.*", "id_rsa.*", "id_ed25519.*"]
        .into_iter()
        .find(|g| hits(g))?;
    (parts.first() != Some(&Part::Star)
        || (prefix_pat == ".env.*" && ENV_VARIANTS.iter().any(|name| glob_match(pattern, name))))
    .then_some(prefix_pat)
}

/// Shell-style match of one path component, case-insensitive. A bracket
/// class is treated as "any one character": wider than the shell, never
/// narrower.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let parts = glob_parts(pattern);
    let name: Vec<char> = name.chars().map(|c| c.to_ascii_lowercase()).collect();
    // Iterative wildcard match with a single backtrack point: linear memory,
    // no recursion, so a hostile pattern cannot blow the stack.
    let (mut p, mut n) = (0, 0);
    let mut backtrack: Option<(usize, usize)> = None;
    while n < name.len() {
        match parts.get(p) {
            Some(Part::Star) => {
                backtrack = Some((p, n));
                p += 1;
            }
            Some(Part::Any) => {
                p += 1;
                n += 1;
            }
            Some(Part::Lit(c)) if *c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match backtrack {
                Some((star, at)) => {
                    p = star + 1;
                    n = at + 1;
                    backtrack = Some((star, at + 1));
                }
                None => return false,
            },
        }
    }
    parts[p.min(parts.len())..]
        .iter()
        .all(|part| *part == Part::Star)
}
