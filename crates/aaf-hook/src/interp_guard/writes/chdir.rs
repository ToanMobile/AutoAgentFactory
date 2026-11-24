//! Paths a script writes relative to a directory it changes into
//! (`os.chdir(d)`, `process.chdir(d)`, `Dir.chdir(d)`): each relative write
//! is judged below each such directory too.

use std::collections::HashSet;

/// Most directory × relative-path pairs analysed. A script naming hundreds
/// of each is denied instead of judged pair by pair.
pub(super) const MAX_CHDIR_PAIRS: usize = 100_000;

#[cfg(test)]
thread_local! {
    pub(super) static PAIRS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// `relative` joined below each of `dirs`, each distinct text once (first
/// spelling kept); `None` when there are too many pairs to analyse.
pub(super) fn below_dirs(dirs: &[String], relative: &[String]) -> Option<Vec<String>> {
    let dirs = distinct(dirs.iter().map(|dir| dir.trim().trim_end_matches('/')));
    let relative = distinct(relative.iter().map(String::as_str));
    if dirs.len().saturating_mul(relative.len()) > MAX_CHDIR_PAIRS {
        return None;
    }
    let mut out = Vec::with_capacity(dirs.len() * relative.len());
    for dir in &dirs {
        for path in &relative {
            #[cfg(test)]
            PAIRS.with(|pairs| pairs.set(pairs.get() + 1));
            out.push(format!("{dir}/{path}"));
        }
    }
    Some(out)
}

fn distinct<'a>(items: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut seen = HashSet::new();
    items.filter(|item| seen.insert(*item)).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    /// O11-15: a directory or path named many times is joined once.
    #[test]
    fn repeated_directories_and_paths_are_joined_once() {
        let dirs = strings(&["sub", "sub/", "sub", " sub"]);
        let relative = strings(&["a.txt", "a.txt", "b.txt", "a.txt"]);
        PAIRS.with(|pairs| pairs.set(0));
        let joined = below_dirs(&dirs, &relative).unwrap();
        assert_eq!(joined, strings(&["sub/a.txt", "sub/b.txt"]));
        assert!(PAIRS.with(std::cell::Cell::get) <= 2);
    }

    /// O11-15: 400 directories × 400 paths is too many pairs to analyse.
    #[test]
    fn too_many_pairs_are_refused() {
        let dirs: Vec<String> = (0..400).map(|n| format!("d{n}")).collect();
        let relative: Vec<String> = (0..400).map(|n| format!("f{n}.txt")).collect();
        PAIRS.with(|pairs| pairs.set(0));
        assert!(below_dirs(&dirs, &relative).is_none());
        assert_eq!(PAIRS.with(std::cell::Cell::get), 0);
    }
}
