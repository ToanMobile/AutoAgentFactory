//! Fail-closed checks for runtime-built `find` words and `-ok`/`-okdir {} +`.

use crate::Denial;

use super::RULE;
use super::find::{EXEC_ACTIONS, dash_f, exec_deleter, exec_end, global_option, primary_args};

const NAME_PRIMARIES: &str =
    "-name -iname -path -ipath -wholename -iwholename -regex -iregex -lname -ilname";

pub(super) fn check(
    args: &[String],
    dynamic: &[bool],
    expression: usize,
    piped: bool,
) -> Option<Denial> {
    let deny = |why| Denial::new(RULE, why, "use literal find arguments and end -ok with \\;");
    let is_dyn = |i: usize| dynamic.get(i).copied().unwrap_or(true);
    let (mut at, mut first_root) = (0, None);
    while at < expression {
        let arg = args[at].as_str();
        if arg == "-D" {
            at += 2;
        } else if let Some(glued) = dash_f(arg) {
            if glued.is_empty() || !is_dyn(at) {
                first_root = first_root.or(Some(at + usize::from(glued.is_empty())));
            }
            at += 1 + usize::from(glued.is_empty());
        } else if global_option(arg) {
            at += 1;
        } else {
            first_root = first_root.or(Some(at));
            break;
        }
    }
    if (0..expression).any(|i| is_dyn(i) && Some(i) != first_root) {
        return Some(deny("find has a search-root word built at run time"));
    }
    let (mut destructive, mut dyn_name) = (piped, false);
    at = expression;
    while at < args.len() {
        if is_dyn(at) {
            return Some(deny("find expression has a word built at run time"));
        }
        let tok = args[at].as_str();
        if EXEC_ACTIONS.contains(&tok) {
            let end = exec_end(args, at);
            if matches!(tok, "-ok" | "-okdir") && args.get(end).is_some_and(|w| w == "+") {
                return Some(deny("find -ok/-okdir with {} + does not end on GNU find"));
            }
            let open = end == args.len() || is_dyn(end);
            let expr = |w: &str| w.starts_with('-') || matches!(w, "(" | ")" | "!" | ",");
            let opaque = |i: usize| {
                open || args.get(i + 1).is_some_and(|w| w == "+")
                    || (i + 1..end).any(|j| is_dyn(j) || expr(&args[j]))
            };
            if (at + 1..end).any(|i| is_dyn(i) && opaque(i)) {
                return Some(deny("find -exec/-ok has a word built at run time"));
            }
            destructive |= exec_deleter(&args[at + 1..end]).is_some();
            at = end + 1;
        } else if let Some(n) = primary_args(tok) {
            destructive |= tok == "-delete";
            dyn_name |= NAME_PRIMARIES.split_ascii_whitespace().any(|p| p == tok)
                && at + 1 < args.len()
                && is_dyn(at + 1);
            at += 1 + n;
        } else {
            at += 1;
        }
    }
    (destructive && dyn_name).then(|| deny("destructive find has a name filter built at run time"))
}
