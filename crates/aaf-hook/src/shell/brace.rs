//! Brace expansion of an unquoted word (`{a,b}`, `{x..y}`, `{1..9..2}`), as
//! the shell does it before any other expansion: `.{g..g}it` is `.git`.

use super::stdin::split_assign;
use super::{Lexer, Redirect, Tok};

/// Bound on the words one word expands to. Past it the word is kept as typed
/// and the line is also judged as text the splitter gave up on.
pub(crate) const MAX_WORDS: usize = 64;

/// One character of a word; `true` when it is an unquoted `{`, `,` or `}`.
type Char = (char, bool);

/// Bound on the unquoted `{` of one word: each may nest one level of
/// expansion, so past it the word is refused before any recursion.
const MAX_OPENS: usize = 4 * MAX_WORDS;

/// Bound on the characters scanned and copied while expanding one word, per
/// character of it: enough for [`MAX_WORDS`] words of its length several
/// times over, while a word crafted to rescan itself is refused in time.
const WORK_PER_CHAR: usize = 8 * MAX_WORDS;

/// The words `text` expands to; `active` holds the byte offsets, in order,
/// of its unquoted `{`, `,` and `}`. `None` past [`MAX_WORDS`],
/// [`MAX_OPENS`] or the work [`WORK_PER_CHAR`] allows.
pub(super) fn expand(text: &str, active: &[usize]) -> Option<Vec<String>> {
    let opens = active
        .iter()
        .filter(|&&at| text.as_bytes().get(at) == Some(&b'{'))
        .count();
    if opens > MAX_OPENS {
        return None;
    }
    let mut next = active.iter().peekable();
    let word: Vec<Char> = text
        .char_indices()
        .map(|(at, c)| {
            while next.next_if(|&&a| a < at).is_some() {}
            (c, next.next_if(|&&a| a == at).is_some())
        })
        .collect();
    let mut out = Vec::new();
    let mut budget = WORK_PER_CHAR.saturating_mul(word.len() + 1);
    expand_into(&word, &mut out, &mut budget).ok()?;
    Some(
        out.into_iter()
            .map(|chars| chars.into_iter().map(|(c, _)| c).collect::<String>())
            // An alternative that leaves nothing (`{,x}` alone) is no word.
            .filter(|word| !word.is_empty())
            .collect(),
    )
}

impl Lexer<'_, '_> {
    /// A character of an unquoted word with no meaning of its own here; a
    /// `{`, `,` or `}` outside `${…}` is noted for brace expansion.
    pub(super) fn push_plain(&mut self, c: char) {
        let param = self.param_braces > 0;
        match c {
            '{' if param => self.param_braces += 1,
            '}' if param => self.param_braces -= 1,
            '{' | ',' | '}' if !param => {
                let word = self.word_mut();
                let at = word.text.len();
                word.braces.push(at);
            }
            _ => {}
        }
        self.word_mut().text.push(c);
    }

    pub(super) fn push_word(&mut self, tok: Tok) {
        // An assignment before the command name is not brace-expanded.
        let assigns = |tok: &Tok| split_assign(tok).is_some();
        if assigns(&tok) && self.toks.iter().all(assigns) {
            self.toks.push(tok);
        } else {
            let words = self.brace_words(tok);
            self.toks.extend(words);
        }
    }

    /// A redirection to `{a,b}` is to each of them (zsh opens them all).
    pub(super) fn push_redirect(&mut self, op: String, tok: Tok) {
        for target in self.brace_words(tok) {
            self.redirects.push(Redirect {
                op: op.clone(),
                target: target.text,
            });
        }
    }

    fn brace_words(&mut self, tok: Tok) -> Vec<Tok> {
        if tok.braces.is_empty() {
            return vec![tok];
        }
        match expand(&tok.text, &tok.braces) {
            Some(words) => {
                // A word that came out of an expansion is never a reserved word.
                let expanded = words.len() != 1 || words.first() != Some(&tok.text);
                words
                    .into_iter()
                    .map(|text| Tok {
                        text,
                        dynamic: tok.dynamic,
                        quoted: tok.quoted || expanded,
                        braces: Vec::new(),
                    })
                    .collect()
            }
            None => {
                if !self.braces_refused {
                    self.braces_refused = true;
                    self.out.opaque.push(self.chars.iter().collect());
                }
                vec![tok]
            }
        }
    }
}

/// The expansion would be past [`MAX_WORDS`] or the work allowed.
struct Overflow;

/// Takes `work` characters scanned or copied from what is left.
fn spend(budget: &mut usize, work: usize) -> Result<(), Overflow> {
    *budget = budget.checked_sub(work).ok_or(Overflow)?;
    Ok(())
}

fn expand_into(
    word: &[Char],
    out: &mut Vec<Vec<Char>>,
    budget: &mut usize,
) -> Result<(), Overflow> {
    let Some((open, close, alternatives)) = first_brace(word, budget)? else {
        if out.len() >= MAX_WORDS {
            return Err(Overflow);
        }
        spend(budget, word.len())?;
        out.push(word.to_vec());
        return Ok(());
    };
    for alt in alternatives {
        spend(budget, open + alt.len() + word.len() - close)?;
        let mut next = word[..open].to_vec();
        next.extend(alt);
        next.extend_from_slice(&word[close + 1..]);
        expand_into(&next, out, budget)?;
    }
    Ok(())
}

/// The first `{` that opens an expansion: its index, the index of its `}`
/// and the alternatives it stands for. A `{` that opens none is literal.
#[allow(clippy::type_complexity)]
fn first_brace(
    word: &[Char],
    budget: &mut usize,
) -> Result<Option<(usize, usize, Vec<Vec<Char>>)>, Overflow> {
    spend(budget, word.len())?;
    for open in (0..word.len()).filter(|&at| word[at] == ('{', true)) {
        let mut depth = 0usize;
        let mut commas = Vec::new();
        let mut close = None;
        for (at, &(c, active)) in word.iter().enumerate().skip(open + 1) {
            match (c, active) {
                ('{', true) => depth += 1,
                ('}', true) if depth == 0 => {
                    close = Some(at);
                    break;
                }
                ('}', true) => depth -= 1,
                (',', true) if depth == 0 => commas.push(at),
                _ => {}
            }
        }
        spend(budget, close.unwrap_or(word.len()) - open)?;
        let Some(close) = close else { continue };
        if !commas.is_empty() {
            let mut alternatives = Vec::with_capacity(commas.len() + 1);
            let mut from = open + 1;
            for at in commas.into_iter().chain([close]) {
                alternatives.push(word[from..at].to_vec());
                from = at + 1;
            }
            return Ok(Some((open, close, alternatives)));
        }
        let inner: String = word[open + 1..close].iter().map(|(c, _)| c).collect();
        if let Some(items) = sequence(&inner)? {
            let alternatives = items
                .into_iter()
                .map(|item| item.chars().map(|c| (c, false)).collect())
                .collect();
            return Ok(Some((open, close, alternatives)));
        }
    }
    Ok(None)
}

/// `x..y[..step]` with two integers or two single letters.
fn sequence(inner: &str) -> Result<Option<Vec<String>>, Overflow> {
    let parts: Vec<&str> = inner.split("..").collect();
    let (from, to, step) = match parts.as_slice() {
        [from, to] => (*from, *to, None),
        [from, to, step] => (*from, *to, Some(*step)),
        _ => return Ok(None),
    };
    let step = match step {
        None => 1,
        Some(step) if is_int(step) => step
            .trim_start_matches(['+', '-'])
            .parse::<u64>()
            .map_err(|_| Overflow)?
            .max(1),
        Some(_) => return Ok(None),
    };
    if is_int(from) && is_int(to) {
        let (Ok(a), Ok(b)) = (from.parse::<i64>(), to.parse::<i64>()) else {
            return Err(Overflow);
        };
        if a.abs_diff(b) / step >= MAX_WORDS as u64 {
            return Err(Overflow);
        }
        let padded = |s: &str| s.trim_start_matches(['+', '-']).starts_with('0') && s.len() > 1;
        let width = if padded(from) || padded(to) {
            from.len().max(to.len())
        } else {
            0
        };
        let mut items = Vec::new();
        let mut n = a;
        loop {
            items.push(format!("{n:0width$}"));
            if n == b {
                break;
            }
            let next = if a < b {
                n.saturating_add_unsigned(step)
            } else {
                n.saturating_sub_unsigned(step)
            };
            if (a < b && next > b) || (a > b && next < b) {
                break;
            }
            n = next;
        }
        return Ok(Some(items));
    }
    let letter = |s: &str| {
        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_ascii_alphabetic() => Some(c as u8),
            _ => None,
        }
    };
    let (Some(a), Some(b)) = (letter(from), letter(to)) else {
        return Ok(None);
    };
    let step = usize::try_from(step).map_err(|_| Overflow)?;
    let items = if a <= b {
        (a..=b)
            .step_by(step)
            .map(|c| char::from(c).to_string())
            .collect()
    } else {
        (b..=a)
            .rev()
            .step_by(step)
            .map(|c| char::from(c).to_string())
            .collect()
    };
    Ok(Some(items))
}

fn is_int(s: &str) -> bool {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_active(text: &str) -> Vec<usize> {
        text.char_indices()
            .filter(|(_, c)| matches!(c, '{' | ',' | '}'))
            .map(|(at, _)| at)
            .collect()
    }

    fn words(text: &str) -> Option<Vec<String>> {
        expand(text, &all_active(text))
    }

    #[test]
    fn expands_like_the_shell() {
        let cases: &[(&str, &[&str])] = &[
            ("a{b,c}d", &["abd", "acd"]),
            ("{a..c}", &["a", "b", "c"]),
            ("{3..1}", &["3", "2", "1"]),
            ("{1..7..3}", &["1", "4", "7"]),
            ("{01..03}", &["01", "02", "03"]),
            ("x{a,{b,c}}", &["xa", "xb", "xc"]),
            ("{a,b}{1,2}", &["a1", "a2", "b1", "b2"]),
            (".git{,.bak}", &[".git", ".git.bak"]),
            ("{}", &["{}"]),
            ("@{u}", &["@{u}"]),
            ("{a", &["{a"]),
            ("{a{b,c}", &["{ab", "{ac"]),
            ("{a..1}", &["{a..1}"]),
            ("{ab..c}", &["{ab..c}"]),
        ];
        for (text, want) in cases {
            assert_eq!(words(text).unwrap(), *want, "{text}");
        }
    }

    #[test]
    fn refuses_past_the_bound() {
        assert!(words("{1..65}").is_none());
        assert_eq!(words("{1..64}").unwrap().len(), MAX_WORDS);
        assert!(words("{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}").is_none());
        assert!(words("{1..99999999999999999999999}").is_none());
        assert!(words(&"{,}".repeat(40)).is_none());
    }

    /// O11-10: a word with more `{` than any expansion needs is refused up
    /// front, not recursed into.
    #[test]
    fn refuses_deep_words_without_recursing() {
        let started = std::time::Instant::now();
        assert!(words(&"{1..1}".repeat(100_000)).is_none());
        assert!(words(&"{a".repeat(100_000)).is_none());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(
            words(&"{1..1}".repeat(MAX_OPENS)).unwrap(),
            ["1".repeat(MAX_OPENS)]
        );
    }

    /// O11-9: the offsets are bytes; a non-ASCII character before a brace
    /// keeps them lined up, and many commas stay linear.
    #[test]
    fn offsets_are_bytes() {
        assert_eq!(words("é{a,b}").unwrap(), ["éa", "éb"]);
        assert_eq!(words("{é,ü}x{1..2}").unwrap(), ["éx1", "éx2", "üx1", "üx2"]);
        let started = std::time::Instant::now();
        let commas = "a,".repeat(40_000);
        assert_eq!(words(&commas).unwrap(), [commas]);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    /// R5-H6-4: a word that would rescan and copy itself for every one of
    /// many expansions is refused within the work bound.
    #[test]
    fn refuses_words_past_the_work_bound() {
        let word = format!(
            "{}{}{}{}",
            "{".repeat(120),
            "{a,b}".repeat(6),
            "{1..1}".repeat(120),
            "x".repeat(30_000)
        );
        let started = std::time::Instant::now();
        assert!(words(&word).is_none());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let wide = format!("{}{}", "{a,b}".repeat(6), "x".repeat(30_000));
        assert_eq!(words(&wide).unwrap().len(), MAX_WORDS);
    }

    #[test]
    fn quoted_braces_stay_literal() {
        assert_eq!(expand("{a,b}", &[]).unwrap(), ["{a,b}"]);
        // Only the outer pair is unquoted: `{'a,b'}` has no comma to split.
        assert_eq!(expand("{a,b}", &[0, 4]).unwrap(), ["{a,b}"]);
    }
}
