//! The parts a written path joins below a part filled in at run time:
//! `os.path.join(d, 'hooks', 'x')`, `d + '/config'`, `Path(d) / 'config'`,
//! `f'{d}/config'`. Each is given as the text to append to what `d` may
//! be (`/hooks`, `/hooks/x`, `/config`), or a suffix glued to it (`.bak`):
//! a literal only compared, tested or replaced is not one of them, nor one
//! before every run-time part (`join('src', f)`). Only the whole path is
//! given, not the directories on the way (`join(d, 'logs', '.keep')` gives
//! `/logs/.keep`), unless a run-time part follows one (`join(d, 'hooks',
//! name)` gives `/hooks`).

use std::collections::HashSet;

use super::{KEYWORDS, MAX_HOPS, Script, literal};

/// Calls that join their arguments into one path below the first.
const BUILDERS: &[&str] = &[
    "join",
    "joinpath",
    "Path",
    "PurePath",
    "PosixPath",
    "resolve",
    "expand_path",
];

/// What [`Script::below`] found.
#[derive(Default)]
pub(in super::super) struct Below {
    /// The parts joined below a run-time part.
    pub(in super::super) tails: Vec<String>,
    /// A run-time part is joined below another (`join(d, f)`): both may
    /// come from a table of the script's own literals.
    pub(in super::super) pair: bool,
}

#[derive(PartialEq)]
enum Kind {
    /// Parentheses around a value, or the whole expression.
    Group,
    /// A call that joins its arguments into a path.
    Builder,
    /// Any other call, a subscript or a list: what it gives is not known.
    Opaque,
}

struct Frame {
    kind: Kind,
    /// A part filled in at run time came before, at this level.
    runtime: bool,
    /// What was joined below it since.
    suffix: String,
    /// The operator right before: `+`, `/`, `,`, or ` ` for none.
    op: char,
    /// Where in the tails the path joined so far is: a part joined below
    /// it replaces it.
    last: Option<usize>,
}

impl Frame {
    fn new(kind: Kind) -> Frame {
        let op = if kind == Kind::Builder { ',' } else { ' ' };
        Frame {
            kind,
            runtime: false,
            suffix: String::new(),
            op,
            last: None,
        }
    }

    /// A builder of the parts of a list it is handed (`'/'.join([d, 'x'])`,
    /// `join(d, *['a', 'b'])`), joined below what came before.
    fn list_of(top: &Frame) -> Frame {
        Frame {
            kind: Kind::Builder,
            runtime: top.runtime,
            suffix: top.suffix.clone(),
            op: ',',
            last: top.last,
        }
    }

    /// The next value is joined to what came before.
    fn joined(&self) -> bool {
        matches!(self.op, '+' | '/') || (self.kind == Kind::Builder && self.op == ',')
    }

    /// The value just read is filled in at run time.
    fn filled(&mut self, out: &mut Below) {
        out.pair |= self.runtime && self.joined();
        self.runtime = true;
        self.suffix.clear();
        self.op = ' ';
        self.last = None;
    }

    /// A literal: below the run-time part when joined to it.
    fn literal(&mut self, lit: &str, out: &mut Below) {
        if self.runtime && self.joined() {
            if self.op != '+' && !self.suffix.ends_with('/') && !lit.starts_with('/') {
                self.suffix.push('/');
            }
            self.suffix.push_str(lit);
            match self.last.and_then(|at| out.tails.get_mut(at)) {
                Some(tail) => tail.clone_from(&self.suffix),
                None => {
                    self.last = Some(out.tails.len());
                    out.tails.push(self.suffix.clone());
                }
            }
        }
        self.op = ' ';
    }

    /// A variable of known values: each joined below the run-time part.
    fn values(&mut self, lits: &[String], out: &mut Below) {
        if !(self.runtime && self.joined()) {
            self.op = ' ';
            return;
        }
        let (suffix, op, last) = (self.suffix.clone(), self.op, self.last);
        for (at, lit) in lits.iter().enumerate() {
            self.suffix.clone_from(&suffix);
            self.op = op;
            self.last = last.filter(|_| at == 0);
            self.literal(lit, out);
        }
    }
}

impl Script<'_> {
    /// Appends to `out` the parts `expr` joins below a part filled in at
    /// run time; `seen` holds the expressions already looked at.
    pub(in super::super) fn below(
        &self,
        expr: &str,
        hops: usize,
        seen: &mut HashSet<String>,
        out: &mut Below,
    ) {
        if !seen.insert(expr.to_string()) {
            return;
        }
        let chars: Vec<char> = expr.chars().collect();
        let mut frames = vec![Frame::new(Kind::Group)];
        let mut at = 0;
        while at < chars.len() {
            let c = chars[at];
            let next = chars.get(at + 1).copied();
            let word_start = c.is_alphabetic()
                || matches!(c, '_' | '$' | '@')
                || (c == '.' && next.is_some_and(|n| n.is_alphabetic() || n == '_'));
            if matches!(c, '\'' | '"' | '`') {
                let (lit, end) = literal(&chars, at);
                at = end;
                let Some(top) = frames.last_mut() else { break };
                match interpolated(&lit) {
                    Some(tails) => {
                        out.pair |= tails.len() > 1;
                        for tail in tails.iter().filter(|tail| !tail.is_empty()) {
                            out.tails.push(tail.clone());
                        }
                        top.filled(out);
                        top.suffix = tails.last().cloned().unwrap_or_default();
                    }
                    None => top.literal(&lit, out),
                }
                continue;
            }
            if word_start {
                let start = at;
                at += 1;
                while at < chars.len()
                    && (chars[at].is_alphanumeric()
                        || chars[at] == '_'
                        || (chars[at] == '.'
                            && chars
                                .get(at + 1)
                                .is_some_and(|n| n.is_alphabetic() || *n == '_')))
                {
                    at += 1;
                }
                let chain: String = chars[start..at].iter().collect();
                let mut after = at;
                while chars.get(after).is_some_and(|c| c.is_whitespace()) {
                    after += 1;
                }
                let method = chain.starts_with('.');
                let name = chain.rsplit('.').next().unwrap_or("");
                // `[d, 'x'].join('/')`: the separator is not a part.
                let joins_list = method && name == "join" && before(&chars, start) == Some(']');
                let Some(top) = frames.last_mut() else { break };
                if chars.get(after) == Some(&'(') {
                    at = after + 1;
                    let mut frame = Frame::new(if BUILDERS.contains(&name) && !joins_list {
                        Kind::Builder
                    } else {
                        Kind::Opaque
                    });
                    if method && frame.kind == Kind::Builder {
                        // `Path(d).joinpath('x')` joins below what it is called on.
                        frame.runtime = top.runtime;
                        frame.suffix = top.suffix.clone();
                        frame.last = top.last;
                    }
                    frames.push(frame);
                    continue;
                }
                if KEYWORDS.contains(&chain.as_str()) {
                    top.op = ' ';
                } else if method || chain.contains('.') {
                    // An attribute: `e.name`, `sys.argv`.
                    top.filled(out);
                } else {
                    let found = self.variable(&chain, hops);
                    if hops < MAX_HOPS {
                        for value in self.assigned(&chain) {
                            self.below(value, hops + 1, seen, out);
                        }
                    }
                    let Some(top) = frames.last_mut() else { break };
                    if found.sure {
                        top.values(&found.lits, out);
                    } else {
                        top.filled(out);
                    }
                }
                continue;
            }
            // A list handed whole to a builder: `sep.join([d, 'x'])`,
            // `join(d, *['a', 'b'])`.
            let listed = matches!(c, '(' | '[')
                && frames.last().is_some_and(|top| top.kind == Kind::Builder)
                && matches!(before(&chars, at), Some('(' | '*'));
            let joined_list = c == '[' && joined_after(&chars, at);
            at += 1;
            match c {
                _ if listed => {
                    let Some(top) = frames.last() else { break };
                    let frame = Frame::list_of(top);
                    frames.push(frame);
                }
                _ if joined_list => frames.push(Frame::new(Kind::Builder)),
                '(' => frames.push(Frame::new(Kind::Group)),
                '[' | '{' => frames.push(Frame::new(Kind::Opaque)),
                ')' | ']' | '}' if frames.len() > 1 => {
                    let Some(done) = frames.pop() else { break };
                    let Some(top) = frames.last_mut() else { break };
                    if done.kind == Kind::Opaque {
                        top.filled(out);
                    } else if done.runtime {
                        top.runtime = true;
                        top.suffix = done.suffix;
                        top.op = ' ';
                        top.last = done.last;
                    } else {
                        top.op = ' ';
                    }
                }
                _ => {
                    let Some(top) = frames.last_mut() else { break };
                    match c {
                        '+' | '/' => top.op = c,
                        // `join(d, *parts)`: still the next part.
                        '*' if top.kind == Kind::Builder && top.op == ',' => {}
                        ',' if top.kind == Kind::Builder => top.op = ',',
                        ',' => {
                            // The next argument of a call that does not join.
                            top.runtime = false;
                            top.suffix.clear();
                            top.op = ' ';
                        }
                        c if c.is_whitespace() => {}
                        _ => top.op = ' ',
                    }
                }
            }
        }
    }
}

/// The last character before `at` that is not blank.
fn before(chars: &[char], at: usize) -> Option<char> {
    chars[..at]
        .iter()
        .rev()
        .find(|c| !c.is_whitespace())
        .copied()
}

/// The list opening at `at` is joined into a path: `[d, 'x'].join('/')`.
fn joined_after(chars: &[char], mut at: usize) -> bool {
    let mut depth = 0usize;
    while at < chars.len() {
        match chars[at] {
            '\'' | '"' | '`' => {
                at = literal(chars, at).1;
                continue;
            }
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let rest = chars[at + 1..].iter().skip_while(|c| c.is_whitespace());
                    return rest.take(6).copied().eq(".join(".chars());
                }
            }
            _ => {}
        }
        at += 1;
    }
    false
}

/// Where the next name is interpolated into a literal: `{`, `$`, or a
/// `%s`/`%r` that `%` formatting fills.
fn marker(text: &str) -> Option<usize> {
    text.char_indices().find_map(|(at, c)| {
        let format = c == '%' && text[at + 1..].starts_with(['s', 'r']);
        (matches!(c, '{' | '$') || format).then_some(at)
    })
}

/// For a literal with names interpolated into it (`f'{d}/config'`,
/// `` `${d}/x` ``, `"$d/x"`, `'{}/x'.format(d)`, `'%s/x' % d`): the text
/// after each.
fn interpolated(lit: &str) -> Option<Vec<String>> {
    let mut tails = Vec::new();
    let mut rest = lit;
    while let Some(at) = marker(rest) {
        let tail = &rest[at..];
        let len = if tail.starts_with('%') {
            2
        } else if let Some(inner) = tail.strip_prefix("${").or_else(|| tail.strip_prefix('{')) {
            match inner.find('}') {
                Some(end) => tail.len() - inner.len() + end + 1,
                None => break,
            }
        } else {
            let word = tail[1..]
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(tail.len() - 1);
            if word == 0 {
                rest = &tail[1..];
                continue;
            }
            1 + word
        };
        rest = &tail[len..];
        let end = marker(rest).unwrap_or(rest.len());
        tails.push(rest[..end].to_string());
    }
    (!tails.is_empty()).then_some(tails)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn found(code: &str, expr: &str) -> Below {
        let script = Script::new(code);
        let mut out = Below::default();
        script.below(expr, 0, &mut HashSet::new(), &mut out);
        out
    }

    fn below(code: &str, expr: &str) -> Vec<String> {
        found(code, expr).tails
    }

    #[test]
    fn joins_below_a_run_time_part() {
        assert_eq!(below("", "os.path.join(d, 'hooks', 'x')"), ["/hooks/x"]);
        assert_eq!(below("", "os.path.join(d, 'hooks', name)"), ["/hooks"]);
        assert_eq!(
            below("", "os.path.join(d, 'logs', '.keep')"),
            ["/logs/.keep"]
        );
        assert_eq!(below("", "(Path(d) / 'hooks').joinpath('x')"), ["/hooks/x"]);
        assert_eq!(below("", "d + '/config'"), ["/config"]);
        assert_eq!(below("", "(d / 'config')"), ["/config"]);
        assert_eq!(below("", "Path(d).joinpath('config')"), ["/config"]);
        assert_eq!(below("", "f'{d}/config'"), ["/config"]);
        assert_eq!(below("", "os.path.join(root, f + '.bak')"), [".bak"]);
        assert_eq!(below("p = os.path.join(r, 'HEAD')", "p"), ["/HEAD"]);
        assert_eq!(below("C = 'config'", "os.path.join(d, C)"), ["/config"]);
        assert_eq!(below("", "'%s/hooks/x' % d"), ["/hooks/x"]);
        assert_eq!(below("", "'/'.join([d, 'config'])"), ["/config"]);
        assert_eq!(below("", "os.sep.join((d, 'config'))"), ["/config"]);
        assert_eq!(below("", "os.path.join(d, *['a', 'b'])"), ["/a/b"]);
        assert_eq!(below("", "[d, 'config'].join('/')"), ["/config"]);
        assert!(below("", "[a, b].join('/')").is_empty());
    }

    #[test]
    fn two_run_time_parts_joined_are_a_pair() {
        for expr in [
            "os.path.join(d, f)",
            "d + '/' + f",
            "f'{d}/{f}'",
            "'/'.join([d, f])",
            "os.path.join(d, *rest)",
        ] {
            assert!(found("", expr).pair, "{expr}");
        }
        for expr in [
            "os.path.join('src', f)",
            "os.path.join(d, 'x')",
            "d.replace(a, b)",
        ] {
            assert!(!found("", expr).pair, "{expr}");
        }
    }

    #[test]
    fn literals_not_below_a_run_time_part() {
        assert!(below("", "os.path.join('src/hooks', f)").is_empty());
        assert!(below("", "p.replace('hooks', 'handlers')").is_empty());
        assert!(below("", "'/hooks/' + name").is_empty());
        assert!(below("p = os.path.join(root, f)", "p").is_empty());
        assert!(below("base = 'out'", "os.path.join(base, 'x')").is_empty());
    }
}
