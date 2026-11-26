//! Patch-file awareness for the static checks.
//!
//! A committed `.patch` / `.diff` file is new content in full, but only its
//! `+` lines are code the patch adds. Context (` `) and removed (`-`) lines,
//! hunk headers and file headers quote code that already lives elsewhere, so
//! code-quality rules must not judge them. Secret rules still do: the patch
//! file itself carries whatever it quotes into the repository.

/// `true` for files whose lines are a unified diff.
pub(super) fn is_patch_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| matches!(ext.to_ascii_lowercase().as_str(), "patch" | "diff"))
}

/// `true` when `text` is a line the patch adds (`+…`, but not the `+++` header).
pub(super) fn is_added_patch_line(text: &str) -> bool {
    text.starts_with('+') && !text.starts_with("+++")
}

/// Rule ids that keep scanning quoted (non-added) patch lines.
pub(super) fn applies_to_quoted_patch_line(rule_id: &str) -> bool {
    rule_id.starts_with("SEC-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_patch_paths_and_added_lines() {
        assert!(is_patch_path(".agents/local/red-patches/BUG-1.patch"));
        assert!(is_patch_path("x/FIX.DIFF"));
        assert!(!is_patch_path("src/patch.rs"));
        assert!(!is_patch_path("patch"));
        assert!(is_added_patch_line("+    val x = 1"));
        assert!(!is_added_patch_line("+++ b/src/A.kt"));
        assert!(!is_added_patch_line("     val x = 1"));
        assert!(!is_added_patch_line("-    val x = 1"));
        assert!(applies_to_quoted_patch_line("SEC-001"));
        assert!(!applies_to_quoted_patch_line("ERR-001"));
    }
}
