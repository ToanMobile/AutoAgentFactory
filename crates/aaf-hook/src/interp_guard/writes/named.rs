//! The literals of a script that are judged when what it writes is not
//! known, and the tables a written path may be joined from.
//!
//! What a copy or a glob leaves out is not written: the names handed to
//! `ignore_patterns(…)` and the values of `ignore`/`exclude`/`ignored`
//! (`ignore=['dist/**']`, `{ignore: '**/.git/**'}`).

use super::parse::literal;

/// Keyword arguments and object keys whose value is what is left out.
const IGNORED: &[&str] = &["ignore", "exclude", "ignored"];

/// Calls whose arguments are only tested against or left out.
const TESTERS: &[&str] = &[
    "remove",
    "discard",
    "includes",
    "startswith",
    "endswith",
    "ignore_patterns",
];

/// One item of a group so far.
enum Item {
    Empty,
    Lit(String),
    Other,
}

/// An open tuple, list, dict or argument list.
struct Group {
    items: Vec<Option<String>>,
    item: Item,
    /// Only tested against: an operand of `in`, `==`, `!=`, or the
    /// arguments of `.remove(`, `.discard(`, `ignore_patterns(`.
    tested: bool,
    /// Inside `ignore_patterns(…)`: its names are not written.
    ignoring: bool,
    /// Assigned to a name (`SKIP = {…}`) or handed to `set(`/`Set(`/
    /// `frozenset(`: with no `:` and no group in it, a list of names.
    hoisted: bool,
    /// Holds a `:` (a dict) or a group of its own (a table).
    table: bool,
}

impl Group {
    fn new(tested: bool, ignoring: bool, hoisted: bool) -> Group {
        Group {
            items: Vec::new(),
            item: Item::Empty,
            tested,
            ignoring,
            hoisted,
            table: false,
        }
    }

    fn next_item(&mut self) {
        let item = std::mem::replace(&mut self.item, Item::Empty);
        match item {
            Item::Empty => {}
            Item::Lit(lit) => self.items.push(Some(lit)),
            Item::Other => self.items.push(None),
        }
    }

    fn other(&mut self) {
        self.item = Item::Other;
    }

    fn lit(&mut self, lit: String) {
        self.item = match self.item {
            Item::Empty => Item::Lit(lit),
            _ => Item::Other,
        };
    }
}

/// The literals of a script, and its groups of literals.
#[derive(Default)]
pub(super) struct Named {
    /// Every string literal, but what is left out.
    pub(super) lits: Vec<String>,
    /// The tuples, lists, dicts and argument lists not only tested
    /// against: for each item, the literal it is when it is one.
    groups: Vec<Vec<Option<String>>>,
}

impl Named {
    /// Each literal beside a protected directory's name in one group
    /// (`('.git', 'hooks/pre-commit')`, `w('.git', 'config')`,
    /// `{'.git': 'HEAD'}`), joined below it.
    pub(super) fn beside<'a>(&'a self, dir: impl Fn(&'a str) -> Option<&'a str>) -> Vec<String> {
        let mut out = Vec::new();
        for items in &self.groups {
            let lits = || items.iter().flatten();
            for name in lits().filter_map(|lit| dir(lit)) {
                out.extend(
                    lits()
                        .filter(|lit| dir(lit).is_none())
                        .map(|lit| format!("{name}/{}", lit.trim_start_matches("./"))),
                );
            }
        }
        out
    }
}

/// The group opening at `at` is the value of an assignment (`SKIP =
/// ['.git', 'hooks']`) or the argument of `set(`, `Set(`, `frozenset(`
/// (`new Set(['.git'])`): a skip list hoisted out of a loop.
fn hoisted_before(chars: &[char], at: usize) -> bool {
    let mut end = at;
    while end > 0 && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    match end.checked_sub(1).map(|b| chars[b]) {
        Some('=') => !matches!(
            end.checked_sub(2).map(|b| chars[b]),
            Some('=' | '!' | '<' | '>' | ':')
        ),
        Some('(') => matches!(
            word_before(chars, end - 1).as_str(),
            "set" | "Set" | "frozenset"
        ),
        _ => false,
    }
}

/// The word that ends right before `end`, skipping blanks.
fn word_before(chars: &[char], end: usize) -> String {
    let mut end = end;
    while end > 0 && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && (chars[start - 1].is_alphanumeric() || chars[start - 1] == '_') {
        start -= 1;
    }
    chars[start..end].iter().collect()
}

/// The group opening at `at` is an operand of a test (`x in (…)`,
/// `x == […]`), or the arguments of a call that only tests them.
fn tested_before(chars: &[char], at: usize) -> bool {
    let mut end = at;
    while end > 0 && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    let before: String = chars[end.saturating_sub(2)..end].iter().collect();
    let word = word_before(chars, at);
    matches!(before.as_str(), "==" | "!=")
        || (word == "in" && !loop_in(chars, end))
        || (chars[at] == '(' && TESTERS.contains(&word.as_str()))
        || (end > 0
            && chars[end - 1] == '('
            && TESTERS.contains(&word_before(chars, end - 1).as_str()))
}

/// The `in` ending at `end` is a loop's (`for d, f in [...]`), not a test:
/// on its line, a `for` comes after any word that starts a test.
fn loop_in(chars: &[char], end: usize) -> bool {
    let start = chars[..end]
        .iter()
        .rposition(|c| *c == '\n')
        .map_or(0, |at| at + 1);
    let line: String = chars[start..end.saturating_sub(2)].iter().collect();
    let mut last = None;
    for word in line.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if matches!(
            word,
            "for" | "if" | "elif" | "while" | "and" | "or" | "not" | "return"
        ) {
            last = Some(word);
        }
    }
    last == Some("for")
}

/// The group ending right before `at` is tested against what follows
/// (`(…) == x`, `[…].includes(x)`, `(…) in x`).
fn tested_after(chars: &[char], at: usize) -> bool {
    let rest: String = chars[at..]
        .iter()
        .skip_while(|c| c.is_whitespace())
        .take(10)
        .collect();
    let word = |w: &str| {
        rest.strip_prefix(w)
            .is_some_and(|after| !after.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
    };
    rest.starts_with("==")
        || rest.starts_with("!=")
        || rest.starts_with(".includes(")
        || rest.starts_with(".indexOf(")
        || word("in")
        || word("not")
}

/// Where the value of a keyword argument or key that starts at `at` ends:
/// at a `,`, `;` or line end, or the bracket closing what holds it.
fn value_end(chars: &[char], mut at: usize) -> usize {
    let mut depth = 0usize;
    while at < chars.len() {
        match chars[at] {
            '\'' | '"' | '`' => {
                at = literal(chars, at).1;
                continue;
            }
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth == 0 => return at,
            ')' | ']' | '}' => depth -= 1,
            ',' | ';' | '\n' if depth == 0 => return at,
            _ => {}
        }
        at += 1;
    }
    at
}

/// The literals of `code` and its groups, in one pass.
pub(super) fn named(code: &str) -> Named {
    let chars: Vec<char> = code.chars().collect();
    let mut out = Named::default();
    // The script itself, then each group open here.
    let mut open = vec![Group::new(true, false, false)];
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        let nested = open.len() > 1;
        let Some(top) = open.last_mut() else { break };
        match c {
            '\'' | '"' | '`' => {
                let (lit, end) = literal(&chars, at);
                if !lit.is_empty() && !top.ignoring {
                    out.lits.push(lit.clone());
                }
                top.lit(lit);
                at = end;
                continue;
            }
            '(' | '[' | '{' => {
                top.other();
                top.table = true;
                let ignoring =
                    top.ignoring || (c == '(' && word_before(&chars, at) == "ignore_patterns");
                let hoisted = hoisted_before(&chars, at);
                open.push(Group::new(tested_before(&chars, at), ignoring, hoisted));
            }
            ')' | ']' | '}' if nested => {
                if let Some(mut done) = open.pop() {
                    done.next_item();
                    let skip_list = done.hoisted && !done.table;
                    if !done.tested && !done.ignoring && !skip_list && !tested_after(&chars, at + 1)
                    {
                        out.groups.push(done.items);
                    }
                }
            }
            ',' => top.next_item(),
            ':' => {
                top.table = true;
                top.next_item();
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = at;
                while at < chars.len() && (chars[at].is_alphanumeric() || chars[at] == '_') {
                    at += 1;
                }
                top.other();
                let word: String = chars[start..at].iter().collect();
                let mut next = at;
                while chars.get(next).is_some_and(|c| *c == ' ') {
                    next += 1;
                }
                let assigned = match chars.get(next) {
                    Some('=') => chars.get(next + 1) != Some(&'='),
                    Some(':') => true,
                    _ => false,
                };
                if IGNORED.contains(&word.as_str()) && assigned {
                    // `ignore=['dist/**']`: left out, not written.
                    at = value_end(&chars, next + 1);
                }
                continue;
            }
            c if c.is_whitespace() => {}
            _ => top.other(),
        }
        at += 1;
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn git(lit: &str) -> Option<&str> {
        (lit == ".git").then_some(lit)
    }

    #[test]
    fn left_out_names_are_not_named() {
        let found = named(
            "copytree('a', b, ignore=ignore_patterns('.git', 'x'))\nfg.sync(['**/*.ts'], {ignore: ['**/node_modules/**']})\nglob('*', exclude='dist/**')\nopen('y')",
        );
        assert_eq!(found.lits, ["a", "**/*.ts", "*", "y"]);
    }

    #[test]
    fn literals_beside_a_protected_name_are_joined() {
        let found = named(
            "F = [('.git', 'hooks/pre-commit', b)]\nw('.git', 'config')\nm = {'.git': 'HEAD'}\nif x in ('.git', 'logs'): pass\nds.remove('.git', 'index')\nfor d, f in {'.git': 'refs'}.items(): pass\n[n for n in ns if n in ('.git', 'objects')]\nif ['.git', 'refs'].includes(n) {}\nif ('.git', 'info') == t: pass",
        );
        assert_eq!(
            found.beside(git),
            [
                ".git/hooks/pre-commit",
                ".git/config",
                ".git/HEAD",
                ".git/refs"
            ]
        );
    }

    #[test]
    fn hoisted_skip_lists_are_not_tables() {
        let found = named(
            "SKIP = {'.git', 'logs'}\nS = new Set(['.git', 'hooks'])\nF = frozenset(('.git', 'refs'))\nT = [('.git', 'config')]\nU = {'.git': 'HEAD'}\nw('.git', 'index')",
        );
        assert_eq!(
            found.beside(git),
            [".git/config", ".git/HEAD", ".git/index"]
        );
    }
}
