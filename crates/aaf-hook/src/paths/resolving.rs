//! A per-call cache of what paths resolve to, so judging one call asks the
//! file system about each path once.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

thread_local! {
    /// While one call is judged: what each path asked about resolved to,
    /// `None` when it does not. The file system is taken as still meanwhile.
    static RESOLVED: RefCell<Option<HashMap<PathBuf, Option<PathBuf>>>> =
        const { RefCell::new(None) };
}

/// Remembers what paths resolve to for [`real`] until dropped: a script
/// writing a thousand files in one directory resolves that directory once.
pub(crate) struct Resolving {
    outer: bool,
}

impl Resolving {
    pub(crate) fn start() -> Resolving {
        let outer = RESOLVED.with(|cell| {
            let mut cache = cell.borrow_mut();
            let outer = cache.is_none();
            if outer {
                *cache = Some(HashMap::new());
            }
            outer
        });
        Resolving { outer }
    }
}

impl Drop for Resolving {
    fn drop(&mut self) {
        if self.outer {
            RESOLVED.with(|cell| *cell.borrow_mut() = None);
        }
    }
}

#[cfg(test)]
thread_local! {
    /// How often [`canonical`] asked the file system.
    static CANONICAL_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// `path.canonicalize()`, remembered while [`Resolving`].
pub(super) fn canonical(path: &Path) -> Option<PathBuf> {
    if let Some(known) = RESOLVED.with(|cell| cell.borrow().as_ref()?.get(path).cloned()) {
        return known;
    }
    #[cfg(test)]
    CANONICAL_RUNS.with(|runs| runs.set(runs.get() + 1));
    let found = path.canonicalize().ok();
    RESOLVED.with(|cell| {
        if let Some(cache) = cell.borrow_mut().as_mut() {
            cache.insert(path.to_path_buf(), found.clone());
        }
    });
    found
}

/// `path` is already known not to resolve: nothing below it does either.
pub(super) fn known_missing(path: &Path) -> bool {
    RESOLVED.with(|cell| {
        cell.borrow()
            .as_ref()
            .is_some_and(|cache| matches!(cache.get(path), Some(None)))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod resolving_tests {
    use std::path::PathBuf;

    use super::super::real;
    use super::*;

    /// E10: while one call is judged, a thousand files in one missing
    /// directory resolve that directory once, and give what they gave.
    #[test]
    fn real_asks_once_per_path_while_resolving() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let files: Vec<PathBuf> = (0..1000)
            .map(|n| dir.path().join(format!("build/f{n}.txt")))
            .collect();
        let before: Vec<PathBuf> = files.iter().map(|file| real(file)).collect();
        let _resolving = Resolving::start();
        CANONICAL_RUNS.with(|runs| runs.set(0));
        for (file, want) in files.iter().zip(&before) {
            assert_eq!(&real(file), want);
            assert!(want.starts_with(&base));
        }
        // The first file, `build`, the directory: then nothing new.
        assert_eq!(CANONICAL_RUNS.with(std::cell::Cell::get), 3);
    }
}
