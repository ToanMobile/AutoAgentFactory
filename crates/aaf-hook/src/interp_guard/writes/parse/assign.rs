//! The values a script gives its names: `name = value`, and the literals a
//! loop over a literal list takes (`for d in ['.git']`).

use std::collections::HashMap;

use super::is_ident;

#[cfg(test)]
thread_local! {
    /// How often [`assignments`] scanned the script.
    pub(super) static ASSIGNED_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The values `name = …` gives each name anywhere in the script, in order,
/// in one pass: a name is a word not glued to a word, `.`, `$` or `@`
/// before it (`$f` and `@a` are names of their own). Each value runs to the
/// next `;` or line end, found once for all the names before it.
pub(super) fn assignments(code: &str) -> HashMap<&str, Vec<&str>> {
    #[cfg(test)]
    ASSIGNED_RUNS.with(|runs| runs.set(runs.get() + 1));
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut prev: Option<char> = None;
    // The end of the statement the last value was in.
    let mut stop = 0;
    let mut chars = code.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let glued = prev.is_some_and(|p| word(p) || matches!(p, '.' | '$' | '@'));
        prev = Some(c);
        if glued || !(word(c) || matches!(c, '$' | '@')) {
            continue;
        }
        let mut end = at + c.len_utf8();
        while let Some(&(next_at, next)) = chars.peek().filter(|(_, next)| word(*next)) {
            end = next_at + next.len_utf8();
            prev = Some(next);
            chars.next();
        }
        let rest = &code[end..];
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        if value.starts_with(['=', '>', '~']) {
            continue;
        }
        let from = code.len() - value.len();
        if stop < from {
            stop = code[from..]
                .find([';', '\n'])
                .map_or(code.len(), |at| from + at);
        }
        out.entry(&code[at..end])
            .or_default()
            .push(&code[from..stop]);
    }
    loops(code, &mut out);
    out
}

/// `for d in ['a', 'b']`, `for (const d of ["a"])`: the
/// loop variable takes each literal of the list in turn. A list holding
/// anything but literals is left alone.
fn loops<'a>(code: &'a str, out: &mut HashMap<&'a str, Vec<&'a str>>) {
    let bytes = code.as_bytes();
    let glued = |at: Option<usize>| {
        at.and_then(|at| bytes.get(at))
            .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'$'))
    };
    let mut from = 0;
    while let Some(found) = code[from..].find("for") {
        let at = from + found;
        from = at + 3;
        if glued(at.checked_sub(1)) || glued(Some(at + 3)) {
            continue;
        }
        let mut rest = code[at + 3..].trim_start();
        rest = rest.strip_prefix('(').unwrap_or(rest).trim_start();
        for declarer in ["const ", "let ", "var ", "my "] {
            if let Some(after) = rest.strip_prefix(declarer) {
                rest = after.trim_start();
                break;
            }
        }
        let end = rest
            .char_indices()
            .find(|&(at, c)| !(c.is_alphanumeric() || c == '_' || (at == 0 && c == '$')))
            .map_or(rest.len(), |(at, _)| at);
        let name = &rest[..end];
        let rest = rest[end..].trim_start();
        let Some(list) = rest.strip_prefix("in").or_else(|| rest.strip_prefix("of")) else {
            continue;
        };
        if !is_ident(name) || !list.starts_with(char::is_whitespace) {
            continue;
        }
        if let Some(items) = literal_items(list.trim_start()) {
            out.entry(name).or_default().extend(items);
        }
    }
}

/// The items of a list or tuple of literals only (`['a', "b"]`), each with
/// its quotes.
fn literal_items(text: &str) -> Option<Vec<&str>> {
    let close = match text.chars().next()? {
        '[' => ']',
        '(' => ')',
        _ => return None,
    };
    let mut items = Vec::new();
    let mut rest = text[1..].trim_start();
    loop {
        let quote = rest.chars().next()?;
        if !matches!(quote, '\'' | '"' | '`') {
            return None;
        }
        let mut escaped = false;
        let end = rest[1..].char_indices().find_map(|(at, c)| {
            let found = !escaped && c == quote;
            escaped = !escaped && c == '\\';
            found.then_some(at + 1)
        })?;
        items.push(&rest[..=end]);
        rest = rest[end + 1..].trim_start();
        rest = match rest.chars().next()? {
            ',' => rest[1..].trim_start(),
            c if c == close => return Some(items),
            _ => return None,
        };
        if rest.starts_with(close) {
            return Some(items);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn loop_over_literal_list_assigns_each_item() {
        let found = assignments(
            "for d in ['.git', \"x\"]:\n  for (const f of ['a/b',]) {}\nfor e in [x, 'y']:\n  pass\nfor g in ps:\n  pass\nfor h, k in [('a', 'b')]:\n  pass\nfor_x in ['z']",
        );
        assert_eq!(found["d"], ["'.git'", "\"x\""]);
        assert_eq!(found["f"], ["'a/b'"]);
        for absent in ["e", "g", "h", "k", "for_x"] {
            assert!(!found.contains_key(absent), "{absent}");
        }
    }
}
