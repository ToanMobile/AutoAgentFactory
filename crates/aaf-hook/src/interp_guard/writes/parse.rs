//! The calls of an interpreter script and what its path expressions hold.

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

mod assign;
mod below;

#[cfg(test)]
use assign::ASSIGNED_RUNS;
use assign::assignments;
pub(super) use below::Below;

/// Bound on the literals one expression or variable is taken to stand for:
/// past it the rest is not known (`p = p + 'a'` many times over).
const MAX_LITS: usize = 256;

/// Bound on the bytes of a literal built by joining or interpolating:
/// past it the value is not known (a join of joins grows at each hop).
const MAX_LIT_BYTES: usize = 4096;

/// Bound on following `name = value` assignments from a path argument.
const MAX_HOPS: usize = 3;

/// Words that are not variables holding a path.
const KEYWORDS: &[&str] = &[
    "in",
    "for",
    "if",
    "else",
    "elif",
    "not",
    "and",
    "or",
    "is",
    "lambda",
    "return",
    "my",
    "our",
    "local",
    "new",
    "const",
    "let",
    "var",
    "await",
    "async",
    "of",
    "do",
    "end",
    "True",
    "False",
    "None",
    "true",
    "false",
    "null",
    "undefined",
    "nil",
];

pub(super) struct Call {
    pub(super) qual: String,
    pub(super) name: String,
    /// The text of what the method is called on: `Path('x')`, or the value
    /// assigned to the variable it is called on.
    pub(super) receiver: Option<String>,
    pub(super) args: Vec<String>,
}

/// The literals an expression may stand for, and whether they are all of
/// it: `false` when part of it is filled in at run time (a loop variable,
/// a function parameter, an element of a list) and could be anything.
#[derive(Clone, Default)]
pub(super) struct Resolved {
    pub(super) lits: Vec<String>,
    pub(super) sure: bool,
}

impl Resolved {
    /// Keeps at most [`MAX_LITS`] literals; with more, the rest is not
    /// known. Whether some were left out.
    fn cap(&mut self) -> bool {
        let over = self.lits.len() > MAX_LITS;
        if over {
            self.lits.truncate(MAX_LITS);
            self.sure = false;
        }
        over
    }
}

#[cfg(test)]
thread_local! {
    /// How often [`Script::resolve`] ran.
    pub(super) static RESOLVE_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A script, with its assignments gathered once, on first use.
pub(super) struct Script<'a> {
    pub(super) code: &'a str,
    assigned: OnceCell<HashMap<&'a str, Vec<&'a str>>>,
    /// What each variable holds, by name and hops: a name assigned from
    /// itself (`i = i + 1`) is resolved once per depth, not once per way
    /// of reaching it.
    variables: RefCell<HashMap<(String, usize), Rc<Resolved>>>,
}

impl<'a> Script<'a> {
    pub(super) fn new(code: &'a str) -> Script<'a> {
        Script {
            code,
            assigned: OnceCell::new(),
            variables: RefCell::new(HashMap::new()),
        }
    }

    /// The values `name = …` gives `name` anywhere in the script.
    pub(super) fn assigned(&self, name: &str) -> &[&'a str] {
        self.assigned
            .get_or_init(|| assignments(self.code))
            .get(name)
            .map_or(&[], Vec::as_slice)
    }

    /// The string literals of an expression, in order, with the values of
    /// the variables in it and of the names interpolated into its strings
    /// (`f'{d}/x'`, `` `${d}/x` ``, `"$d/x"`). Literals joined as a path
    /// (`Path('a')/'b'`, `os.path.join(d, 'b')`, `d + '/b'`) also count
    /// joined.
    pub(super) fn resolve(&self, expr: &str, hops: usize) -> Resolved {
        #[cfg(test)]
        RESOLVE_RUNS.with(|runs| runs.set(runs.get() + 1));
        let expr = expr.trim();
        if let Some((key, value)) = expr.split_once('=')
            && is_ident(key.trim())
            && !value.starts_with(['=', '>', '~'])
        {
            // A keyword argument: `file=p`, `mode='w'`.
            return self.resolve(value, hops);
        }
        let mut out = Resolved {
            lits: Vec::new(),
            sure: true,
        };
        if expr.is_empty()
            || expr.contains("=>")
            || expr.starts_with("lambda")
            || expr.starts_with("function")
            || expr.parse::<f64>().is_ok()
            || KEYWORDS.contains(&expr)
        {
            // Nothing, a callback, a number or a constant: not a path.
            return out;
        }
        let chars: Vec<char> = expr.chars().collect();
        let mut at = 0;
        while at < chars.len() {
            let c = chars[at];
            if matches!(c, '\'' | '"' | '`') {
                let (lit, end) = literal(&chars, at);
                at = end;
                if !lit.is_empty() {
                    let (lit, sure) = self.interpolate(&lit, hops);
                    out.sure &= sure;
                    out.lits.push(lit);
                }
                continue;
            }
            if c.is_alphabetic() || matches!(c, '_' | '$' | '@') {
                let start = at;
                at += 1;
                while at < chars.len() && (chars[at].is_alphanumeric() || chars[at] == '_') {
                    at += 1;
                }
                let word: String = chars[start..at].iter().collect();
                let before = start.checked_sub(1).map(|b| chars[b]);
                let next = chars[at..].iter().find(|c| !c.is_whitespace());
                let variable = is_ident(&word)
                    && !KEYWORDS.contains(&word.as_str())
                    && before != Some('.')
                    && !before.is_some_and(|b| b.is_ascii_digit())
                    && !matches!(next, Some('(' | '.' | ':' | '\'' | '"' | '`'));
                if variable {
                    let found = self.variable(&word, hops);
                    out.sure &= found.sure;
                    out.lits.extend(found.lits.iter().cloned());
                }
                continue;
            }
            at += 1;
        }
        // A call's result or an attribute (`get_path()`, `sys.argv[1]`):
        // filled in at run time.
        out.sure &= !out.lits.is_empty();
        let bare = unquoted(expr);
        let over = out.cap() || out.lits.len() < 2;
        let joined_len = out.lits.iter().map(|lit| lit.len() + 1).sum::<usize>();
        if !over && joined_len > MAX_LIT_BYTES {
            // The pieces are kept; their join is not known.
            out.sure = false;
        } else if !over {
            if expr.contains("join") || bare.contains('/') {
                out.lits.push(out.lits.join("/"));
            }
            if bare.contains('+') {
                out.lits.push(out.lits.concat());
            }
        }
        out
    }

    /// What a variable holds: the values assigned to it.
    pub(super) fn variable(&self, word: &str, hops: usize) -> Rc<Resolved> {
        let key = (word.to_string(), hops);
        if let Some(found) = self.variables.borrow().get(&key) {
            return Rc::clone(found);
        }
        let mut out = Resolved::default();
        let values = if hops < MAX_HOPS {
            self.assigned(word)
        } else {
            &[]
        };
        out.sure = !values.is_empty();
        for value in values {
            let found = self.resolve(value, hops + 1);
            out.sure &= found.sure;
            out.lits.extend(found.lits);
            if out.cap() {
                break;
            }
        }
        let out = Rc::new(out);
        self.variables.borrow_mut().insert(key, Rc::clone(&out));
        out
    }

    /// A literal with the names interpolated into it replaced by their
    /// value; not sure when one of them has none.
    fn interpolate(&self, lit: &str, hops: usize) -> (String, bool) {
        let mut out = String::new();
        let mut sure = true;
        let mut rest = lit;
        while let Some(at) = rest.find(['{', '$']) {
            out.push_str(&rest[..at]);
            let tail = &rest[at..];
            let (name, len) =
                if let Some(inner) = tail.strip_prefix("${").or_else(|| tail.strip_prefix('{')) {
                    match inner.find('}') {
                        Some(end) => (inner[..end].trim(), tail.len() - inner.len() + end + 1),
                        None => ("", tail.len()),
                    }
                } else {
                    let word = tail[1..]
                        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                        .map_or(&tail[1..], |end| &tail[1..1 + end]);
                    if word.is_empty() {
                        out.push('$');
                        rest = &tail[1..];
                        continue;
                    }
                    (word, 1 + word.len())
                };
            let value = (is_ident(name) && !name.is_empty())
                .then(|| self.variable(name, hops))
                .filter(|found| found.sure)
                .and_then(|found| found.lits.first().cloned());
            match value {
                Some(value) if out.len() + value.len() <= MAX_LIT_BYTES => out.push_str(&value),
                _ => {
                    sure = false;
                    out.push_str(&tail[..len]);
                }
            }
            rest = &tail[len..];
        }
        out.push_str(rest);
        (out, sure)
    }
}

/// The quoted literal starting at `chars[at]` and the index after it.
pub(super) fn literal(chars: &[char], at: usize) -> (String, usize) {
    let q = chars[at];
    let mut buf = String::new();
    let mut i = at + 1;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            buf.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if c == q {
            return (buf, i + 1);
        }
        buf.push(c);
        i += 1;
    }
    (buf, i)
}

pub(super) fn is_ident(text: &str) -> bool {
    let bare = text.trim_start_matches(['$', '@']);
    bare.starts_with(|c: char| c.is_alphabetic() || c == '_')
        && bare.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// `text` with its quoted parts removed.
fn unquoted(text: &str) -> String {
    let mut out = String::new();
    let mut quote = None;
    for c in text.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if matches!(c, '\'' | '"' | '`') => quote = Some(c),
            None => out.push(c),
        }
    }
    out
}

/// Every call `name(…)` in the script; Perl's `open FH, …` and a `chdir`
/// without parentheses.
pub(super) fn calls(script: &Script) -> Vec<Call> {
    let code = script.code;
    let chars: Vec<char> = code.chars().collect();
    let text = |from: usize, to: usize| chars[from..to].iter().collect::<String>();
    let (open_of, close_of) = parens(&chars);
    let chain_before = |end: usize| {
        let mut start = end;
        while start > 0
            && (chars[start - 1].is_alphanumeric() || matches!(chars[start - 1], '_' | '.' | ':'))
        {
            start -= 1;
        }
        start
    };
    // The start of a call chain ending in the `)` at `close`:
    // `Path('a').joinpath('b')`.
    let chain_start = |mut close: usize| loop {
        let Some(open) = open_of[close] else {
            return close;
        };
        let start = chain_before(open);
        let mut end = start;
        while end > 0 && chars[end - 1].is_whitespace() {
            end -= 1;
        }
        match end.checked_sub(1) {
            Some(q) if chars.get(start) == Some(&'.') && chars[q] == ')' => close = q,
            _ => return start,
        }
    };
    let mut out = Vec::new();
    for (at, close) in close_of.iter().enumerate() {
        let Some(close) = *close else { continue };
        let mut end = at;
        while end > 0 && chars[end - 1] == ' ' {
            end -= 1;
        }
        let start = chain_before(end);
        if start == end {
            continue;
        }
        let chain = text(start, end);
        let (qual, name) = match chain.rfind(['.', ':']) {
            Some(dot) => (chain[..dot].to_string(), chain[dot + 1..].to_string()),
            None => (String::new(), chain.clone()),
        };
        let receiver = if qual.is_empty() && chain.starts_with('.') {
            let mut end = start;
            while end > 0 && chars[end - 1].is_whitespace() {
                end -= 1;
            }
            end.checked_sub(1)
                .filter(|q| chars[*q] == ')')
                .map(|q| text(chain_start(q), q + 1))
        } else if is_ident(&qual) {
            let values = script.assigned(&qual);
            (!values.is_empty()).then(|| values.join(" "))
        } else {
            None
        };
        out.push(Call {
            qual,
            name,
            receiver,
            args: split_args(&chars[at + 1..close]),
        });
    }
    for line in code.split([';', '\n']) {
        let line = line.trim_start();
        // `open my $f, '>', 'x';` and `open F, ">x" or die`.
        if let Some(rest) = line.strip_prefix("open ")
            && !rest.trim_start().starts_with('(')
        {
            let chars: Vec<char> = rest.chars().collect();
            out.push(Call {
                qual: String::new(),
                name: "open".to_string(),
                receiver: None,
                args: split_args(&chars),
            });
        }
        // `chdir ".git"` (Perl), `Dir.chdir '.git' do` (Ruby).
        if let Some(at) = line.find("chdir ")
            && (at == 0 || line[..at].ends_with('.'))
            && !line[at + 6..].trim_start().starts_with('(')
        {
            let rest = &line[at + 6..];
            let end = rest.find([' ', '{']).unwrap_or(rest.len());
            out.push(Call {
                qual: String::new(),
                name: "chdir".to_string(),
                receiver: None,
                args: vec![rest[..end].to_string()],
            });
        }
    }
    out
}

/// For each `(` and `)` outside string literals, the index of its partner.
fn parens(chars: &[char]) -> (Vec<Option<usize>>, Vec<Option<usize>>) {
    let mut open_of = vec![None; chars.len()];
    let mut close_of = vec![None; chars.len()];
    let mut stack = Vec::new();
    let mut quote = None;
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        match quote {
            Some(_) if c == '\\' => at += 1,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if matches!(c, '\'' | '"' | '`') => quote = Some(c),
            None if c == '(' => stack.push(at),
            None if c == ')' => {
                if let Some(open) = stack.pop() {
                    open_of[at] = Some(open);
                    close_of[open] = Some(at);
                }
            }
            None => {}
        }
        at += 1;
    }
    (open_of, close_of)
}

/// The arguments of a call: split at commas outside quotes and brackets.
fn split_args(chars: &[char]) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    for &c in chars {
        if escaped {
            escaped = false;
        } else if let Some(q) = quote {
            if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
        } else if matches!(c, '\'' | '"' | '`') {
            quote = Some(c);
        } else if matches!(c, '(' | '[' | '{') {
            depth += 1;
        } else if matches!(c, ')' | ']' | '}') {
            depth = depth.saturating_sub(1);
        } else if c == ',' && depth == 0 {
            out.push(std::mem::take(&mut current));
            continue;
        }
        current.push(c);
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
