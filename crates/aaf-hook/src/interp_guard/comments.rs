//! Quote- and comment-aware stripping helpers for `interp_guard` (SEC-113).

use super::parse::Interp;

/// Line terminators recognized across supported interpreters (POSIX `\n`,
/// Python/JS/PHP `\r`, ECMAScript U+2028/U+2029).
const LINE_BREAKS: [char; 4] = ['\n', '\r', '\u{2028}', '\u{2029}'];

/// True when `ch` terminates a `#`/`//` line comment in `interp_kind`.
fn ends_comment(interp_kind: Interp, ch: char) -> bool {
    match interp_kind {
        Interp::Python | Interp::Php => matches!(ch, '\n' | '\r'),
        Interp::Ruby | Interp::Perl => ch == '\n',
        Interp::Node => LINE_BREAKS.contains(&ch),
    }
}

pub(super) fn strip_comment_lines(code: &str, interp_kind: Interp) -> String {
    code.split(|ch| ends_comment(interp_kind, ch))
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with('#') && !t.starts_with("//")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Naively strips inline trailing `#` (except `#!` shebang lines) and `//`
/// comments per line without tracking quotes, using `interp_kind`'s comment
/// terminators (SEC-113).
pub(super) fn strip_trailing_comments(code: &str, interp_kind: Interp) -> String {
    code.split(|ch| ends_comment(interp_kind, ch))
        .map(|line| {
            let hash = (!line.starts_with("#!")).then(|| line.find('#')).flatten();
            let slash = line.find("//");
            match (hash, slash) {
                (Some(h), Some(s)) => &line[..h.min(s)],
                (Some(i), None) | (None, Some(i)) => &line[..i],
                (None, None) => line,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Copies a single-delimited string (`'...'`, `"..."`, or JS `` `...` ``)
/// starting at `start`, preserving `\` escapes verbatim, and returns the next
/// byte index.
fn copy_quoted(code: &str, start: usize, quote: char, out: &mut String) -> usize {
    out.push(quote);
    let mut i = start + quote.len_utf8();
    while let Some(ch) = code[i..].chars().next() {
        out.push(ch);
        i += ch.len_utf8();
        if ch == '\\' {
            if let Some(esc) = code[i..].chars().next() {
                out.push(esc);
                i += esc.len_utf8();
            }
        } else if ch == quote {
            break;
        }
    }
    i
}

/// Copies a Python triple-quoted string (`'''...'''` or `"""..."""`) starting
/// at `start`, preserving `\` escapes verbatim, and returns the next index.
fn copy_triple_quoted(code: &str, start: usize, delim: &str, out: &mut String) -> usize {
    out.push_str(delim);
    let mut i = start + delim.len();
    while i < code.len() {
        if code[i..].starts_with(delim) {
            out.push_str(delim);
            return i + delim.len();
        }
        let Some(ch) = code[i..].chars().next() else {
            break;
        };
        out.push(ch);
        i += ch.len_utf8();
        if ch == '\\'
            && let Some(esc) = code[i..].chars().next()
        {
            out.push(esc);
            i += esc.len_utf8();
        }
    }
    i
}

/// Single-pass quote- and comment-aware stripper: preserves string literals
/// verbatim so `#` or `//` inside strings never unbalance quotes, while
/// replacing comments outside strings with whitespace or newlines.
pub(super) fn strip_comments_quote_aware(code: &str, interp_kind: Interp) -> String {
    let hash_comment = matches!(
        interp_kind,
        Interp::Python | Interp::Ruby | Interp::Perl | Interp::Php
    );
    let slash_comment = matches!(interp_kind, Interp::Node | Interp::Php);
    let backtick_str = interp_kind == Interp::Node;
    let triple_str = interp_kind == Interp::Python;
    let php = interp_kind == Interp::Php;

    let mut out = String::with_capacity(code.len());
    let mut i = 0;
    while i < code.len() {
        let rest = &code[i..];
        if rest.starts_with("\r\n") {
            out.push('\n');
            i += 2;
            continue;
        }
        let Some(c) = rest.chars().next() else {
            break;
        };
        if LINE_BREAKS.contains(&c) {
            out.push('\n');
            i += c.len_utf8();
            continue;
        }
        if triple_str && (rest.starts_with("'''") || rest.starts_with("\"\"\"")) {
            i = copy_triple_quoted(code, i, &rest[..3], &mut out);
            continue;
        }
        if c == '\'' || c == '"' || (backtick_str && c == '`') {
            i = copy_quoted(code, i, c, &mut out);
            continue;
        }
        if slash_comment && rest.starts_with("/*") {
            i += 2;
            let mut had_break = false;
            while i < code.len() {
                if code[i..].starts_with("*/") {
                    i += 2;
                    break;
                }
                let Some(ch) = code[i..].chars().next() else {
                    break;
                };
                had_break |= LINE_BREAKS.contains(&ch);
                i += ch.len_utf8();
            }
            out.push(if had_break { '\n' } else { ' ' });
            continue;
        }
        let js_shebang = interp_kind == Interp::Node && i == 0 && rest.starts_with("#!");
        let slash_line = slash_comment && rest.starts_with("//");
        if (hash_comment && c == '#') || js_shebang || slash_line {
            i += if js_shebang || slash_line { 2 } else { 1 };
            while let Some(ch) = code[i..].chars().next() {
                if php && code[i..].starts_with("?>") {
                    out.push(' ');
                    break;
                }
                if ends_comment(interp_kind, ch) {
                    break;
                }
                i += ch.len_utf8();
            }
            continue;
        }
        out.push(c);
        i += c.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Interp, strip_comment_lines, strip_comments_quote_aware, strip_trailing_comments};

    #[test]
    fn splits_cr_and_unicode_line_separators() {
        assert_eq!(
            strip_comment_lines("# x'\rimport os", Interp::Python),
            "import os"
        );
        assert_eq!(
            strip_comment_lines("// x'\u{2028}run()", Interp::Node),
            "run()"
        );
        assert_eq!(
            strip_comment_lines("  // x'\u{2029}run()", Interp::Node),
            "run()"
        );
        assert_eq!(strip_comment_lines("# a\r'\nrun()", Interp::Ruby), "run()");
        assert_eq!(
            strip_comment_lines("# a\u{2028}'\nrun()", Interp::Python),
            "run()"
        );
    }

    #[test]
    fn strips_trailing_comments_per_language() {
        assert_eq!(
            strip_trailing_comments("const r = /a\\/*/; // don't\nrun()", Interp::Node),
            "const r = /a\\/*/; \nrun()"
        );
        assert_eq!(
            strip_trailing_comments("x = 1 # a\r'\nrun()", Interp::Ruby),
            "x = 1 \nrun()"
        );
        assert_eq!(
            strip_trailing_comments("#!/usr/bin/env python3\nx = 1 # note", Interp::Python),
            "#!/usr/bin/env python3\nx = 1 "
        );
    }

    #[test]
    fn preserves_strings_and_strips_comments_by_interpreter() {
        for (interp, input, want) in [
            (
                Interp::Python,
                "u = 'http://x' # don't\nrun()",
                "u = 'http://x' \nrun()",
            ),
            (Interp::Python, r#"s = 'a\'#b' # don't"#, r#"s = 'a\'#b' "#),
            (
                Interp::Python,
                "s = '''a # b''' # c\nrun()",
                "s = '''a # b''' \nrun()",
            ),
            (
                Interp::Python,
                "x = 10 // 3 # div\rprint(x)",
                "x = 10 // 3 \nprint(x)",
            ),
            (
                Interp::Node,
                "const u = `a//b` // don't\r\nrun()",
                "const u = `a//b` \nrun()",
            ),
            (Interp::Node, "/* don't */run()", " run()"),
            (Interp::Node, "/*/ don't */run()", " run()"),
            (Interp::Php, "a/*x\ny*/b", "a\nb"),
            (Interp::Php, "/* ?> don't */run()", " run()"),
            (Interp::Php, "// don't ?><?php run();", " ?><?php run();"),
            (Interp::Php, "# don't ?><?php run();", " ?><?php run();"),
            (Interp::Perl, "//;system('x')", "//;system('x')"),
            (
                Interp::Node,
                "#!/usr/bin/env node\nclass A { #x = 1; }",
                "\nclass A { #x = 1; }",
            ),
            (Interp::Node, "a = 1\n#!x", "a = 1\n#!x"),
            (Interp::Python, "a = 1\n#! don't\nrun()", "a = 1\n\nrun()"),
            (Interp::Python, "x = 1 # a\u{2028}'\nrun()", "x = 1 \nrun()"),
            (Interp::Ruby, "x = 1 # a\r'\nrun()", "x = 1 \nrun()"),
            (
                Interp::Perl,
                "my $a = 1; # a\r'\nrun()",
                "my $a = 1; \nrun()",
            ),
            (
                Interp::Php,
                "<?php $a = 1; // a\u{2028}'\nrun();",
                "<?php $a = 1; \nrun();",
            ),
        ] {
            assert_eq!(strip_comments_quote_aware(input, interp), want, "{input:?}");
        }
    }
}
