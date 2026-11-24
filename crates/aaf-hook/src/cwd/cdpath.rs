//! `CDPATH`: where a `cd` to a bare relative name may land, as the line
//! leaves the variable at each command.

use crate::shell;

/// What `CDPATH` holds for the line: the hook's own environment until a
/// command of this shell unsets or empties it for the commands after it,
/// and each value the line gives it.
pub(super) struct Search {
    inherited: Option<String>,
    /// The first command that unsets or empties it.
    cleared_at: Option<usize>,
    given: Vec<String>,
    /// A value is not known (`CDPATH=$X`, `read CDPATH`).
    unknown: bool,
}

impl Search {
    /// The values `CDPATH` may hold at command `idx`, each split into its
    /// directories: `None` when unset, `Some(None)` when a value is not known.
    pub(super) fn at(&self, idx: usize) -> Option<Option<Vec<Vec<String>>>> {
        if self.unknown {
            return Some(None);
        }
        let inherited = self
            .inherited
            .as_ref()
            .filter(|_| self.cleared_at.is_none_or(|at| idx < at));
        let values: Vec<&String> = inherited.into_iter().chain(&self.given).collect();
        if values.is_empty() {
            return None;
        }
        Some(Some(
            values
                .iter()
                .map(|value| value.split(':').map(str::to_string).collect())
                .collect(),
        ))
    }
}

pub(super) fn cdpath(script: &shell::Script) -> Search {
    let mut search = Search {
        inherited: std::env::var("CDPATH")
            .ok()
            .filter(|value| !value.is_empty()),
        cleared_at: None,
        given: Vec::new(),
        unknown: false,
    };
    // A subshell's `unset` or a function body's (run or not) leaves this
    // shell's value alone.
    let mut depth = 0usize;
    let mut scope_at = 0;
    for (idx, cmd) in script.cmds.iter().enumerate() {
        while let Some((_, event)) = script.scopes.get(scope_at).filter(|(at, _)| *at <= idx) {
            scope_at += 1;
            depth = match event {
                shell::ScopeEvent::Enter(_) => depth + 1,
                shell::ScopeEvent::Exit => depth.saturating_sub(1),
            };
        }
        // Only a clear that surely runs in this shell (not after `&&`, in a
        // pipeline or an `if` body) clears it for the commands after it.
        let here = depth == 0 && script.functions.is_empty() && script.plain_steps.contains(&idx);
        let names_function = cmd
            .args()
            .iter()
            .take_while(|arg| arg.starts_with('-') && *arg != "--")
            .any(|arg| arg.contains('f'));
        let unset = cmd.name() == "unset"
            && !names_function
            && cmd.args().iter().any(|arg| arg == "CDPATH");
        if !(unset || cmd.sets_var("CDPATH") || cmd.sets_var("cdpath")) {
            continue;
        }
        let declared = shell::DECLARERS.contains(&cmd.name());
        let literal = cmd
            .assigns
            .iter()
            .filter(|(key, _)| key == "CDPATH")
            .map(|(_, value)| value.as_str())
            .chain(
                cmd.args()
                    .iter()
                    .filter(|_| declared)
                    .filter_map(|arg| arg.strip_prefix("CDPATH=")),
            )
            .collect::<Vec<_>>();
        // `CDPATH= ls` empties it for `ls` alone.
        let bare = cmd.words.is_empty() || declared;
        if here && (unset || (bare && literal.contains(&""))) {
            search.cleared_at.get_or_insert(idx);
        }
        if unset {
            continue;
        }
        search.unknown |=
            literal.is_empty() || literal.iter().any(|value| value.contains(['$', '`']));
        search.given.extend(
            literal
                .into_iter()
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        );
    }
    search
}

/// The directory a `cd`/`pushd` looks up in `CDPATH`: a bare relative name
/// (`cd etc`), not `/…`, `~…`, `.`, `..`, `./…` or `../…`.
pub(super) fn searched_dir(cmd: &shell::Cmd) -> Option<&str> {
    if !matches!(cmd.name(), "cd" | "pushd") {
        return None;
    }
    let mut after_dd = false;
    for arg in cmd.args() {
        if after_dd || !arg.starts_with(['-', '+']) {
            let relative = !arg.starts_with(['/', '~', '$'])
                && !matches!(arg.as_str(), "." | "..")
                && !arg.starts_with("./")
                && !arg.starts_with("../");
            return relative
                .then_some(arg.as_str())
                .filter(|arg| !arg.is_empty());
        }
        if arg == "--" {
            after_dd = true;
        } else if arg == "-n" || arg == "-" {
            return None;
        }
    }
    None
}
