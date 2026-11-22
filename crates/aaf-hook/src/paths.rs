//! Path helpers: lexical resolution, containment and protected-name patterns.

use std::path::{Component, Path, PathBuf};

/// Resolves `raw` the way the shell would for a literal word: `~`, `$HOME`
/// and relative paths, then `.`/`..` without touching the file system.
pub fn expand(raw: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    // `$HOMEWORK` is not `$HOME`: the prefix must end at a separator.
    let home_rest = ["~", "$HOME", "${HOME}"]
        .iter()
        .find_map(|prefix| raw.strip_prefix(prefix))
        .filter(|rest| rest.is_empty() || rest.starts_with('/'));
    let path = match (home_rest, home) {
        (Some(rest), Some(home)) => home.join(rest.trim_start_matches('/')),
        _ => PathBuf::from(raw),
    };
    if path.is_absolute() {
        normalize(&path)
    } else {
        normalize(&cwd.join(path))
    }
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
        if let Ok(resolved) = current.canonicalize() {
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
/// macOS file system compares names.
pub fn protected_name(path: &Path) -> Option<&'static str> {
    let name = fold(path.file_name()?.to_str()?);
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
    NESTED
        .iter()
        .find(|(dir, file)| parent == *dir && name == *file)
        .map(|(_, file)| *file)
}

pub fn has_glob(text: &str) -> bool {
    text.contains(['*', '?', '['])
}

#[derive(PartialEq)]
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
                for inner in chars.by_ref() {
                    if inner == ']' {
                        break;
                    }
                }
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
    ".env.example",
];

/// A file-name glob (as given to `find -name`) can match a protected file.
pub fn may_name_protected(pattern: &str) -> bool {
    if pattern.len() > MAX_PATTERN {
        return true;
    }
    let parts = glob_parts(pattern);
    if PROTECTED_GLOBS
        .iter()
        .any(|glob| globs_intersect(&parts, &glob_parts(glob)))
    {
        return true;
    }
    // ponytail: every `*.<ext>` also matches `.env.<ext>`, so strictly no
    // suffix filter is safe. A pattern opening with `*` is refused only for
    // the usual `.env.*` variants; upgrade to listing the tree if a project
    // keeps secrets in `.env.<unusual>`.
    globs_intersect(&parts, &glob_parts(".env.*"))
        && (parts.first() != Some(&Part::Star)
            || ENV_VARIANTS.iter().any(|name| glob_match(pattern, name)))
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
