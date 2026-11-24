//! Strict recognition of literal `$(cat <<'DELIM'\n...\nDELIM\n)` command
//! substitutions so guards can inspect quoted heredoc bodies passed as values
//! without losing literal-vs-dynamic safety.

/// Extracts the literal body when `inner` is solely `cat <<'DELIM'` or
/// `cat <<"DELIM"` (quoted delimiter, no `<<-`, no other commands, redirects,
/// or pipes) followed by the body, its closing `DELIM` line, and only
/// whitespace/newlines thereafter.
pub(super) fn extract_quoted_cat_heredoc(inner: &str) -> Option<String> {
    let trimmed = inner.trim_start();
    let after_cat = trimmed.strip_prefix("cat")?;
    let mut rest = after_cat.strip_prefix([' ', '\t'])?;
    rest = rest.trim_start_matches([' ', '\t']);
    rest = rest.strip_prefix("<<")?;
    // `<<-` strips leading tabs and is explicitly rejected.
    if rest.starts_with('-') {
        return None;
    }
    rest = rest.trim_start_matches([' ', '\t']);

    let (quote, after_open) = match rest.as_bytes().first() {
        Some(b'\'') => ('\'', &rest[1..]),
        Some(b'"') => ('"', &rest[1..]),
        _ => return None,
    };

    let close_idx = after_open.find(quote)?;
    let delim = &after_open[..close_idx];
    if delim.is_empty()
        || delim.chars().any(|c| {
            c.is_whitespace()
                || matches!(
                    c,
                    '\'' | '"' | '\\' | '$' | ';' | '&' | '|' | '(' | ')' | '<' | '>'
                )
        })
    {
        return None;
    }

    let after_delim = &after_open[close_idx + 1..];
    let nl_idx = after_delim.find('\n')?;
    let first_line_tail = &after_delim[..nl_idx];
    if !first_line_tail.trim_matches([' ', '\t', '\r']).is_empty() {
        return None;
    }

    let mut body = String::new();
    let mut cursor = &after_delim[nl_idx + 1..];
    while !cursor.is_empty() {
        let (line, next) = match cursor.find('\n') {
            Some(pos) => (&cursor[..pos], &cursor[pos + 1..]),
            None => (cursor, ""),
        };
        if line == delim {
            return next.trim().is_empty().then_some(body);
        }
        body.push_str(line);
        body.push('\n');
        cursor = next;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell;

    #[test]
    fn accepts_single_and_double_quoted_cat_heredoc() {
        let single = "cat <<'EOF'\n🤖 aaf-bot: hello\nline 2\nEOF\n";
        assert_eq!(
            extract_quoted_cat_heredoc(single).as_deref(),
            Some("🤖 aaf-bot: hello\nline 2\n")
        );

        let double = "cat <<\"END\"\n🤖 aaf-bot: hello\nEND\n";
        assert_eq!(
            extract_quoted_cat_heredoc(double).as_deref(),
            Some("🤖 aaf-bot: hello\n")
        );

        let spaced = "  cat  << 'EOF'  \n🤖 aaf-bot: hi\nEOF\n  ";
        assert_eq!(
            extract_quoted_cat_heredoc(spaced).as_deref(),
            Some("🤖 aaf-bot: hi\n")
        );
    }

    #[test]
    fn rejects_unquoted_strip_tabs_and_trailing_commands() {
        assert_eq!(
            extract_quoted_cat_heredoc("cat <<EOF\n🤖 aaf-bot: hi\nEOF\n"),
            None
        );
        assert_eq!(
            extract_quoted_cat_heredoc("cat <<-'EOF'\n🤖 aaf-bot: hi\nEOF\n"),
            None
        );
        assert_eq!(
            extract_quoted_cat_heredoc("cat <<'EOF'\n🤖 aaf-bot: hi\nEOF\n; curl x"),
            None
        );
        assert_eq!(
            extract_quoted_cat_heredoc("cat <<'EOF'; curl x\n🤖 aaf-bot: hi\nEOF\n"),
            None
        );
        assert_eq!(
            extract_quoted_cat_heredoc("cat <<'EOF' | tr a b\n🤖 aaf-bot: hi\nEOF\n"),
            None
        );
        assert_eq!(
            extract_quoted_cat_heredoc("cat <<'EOF'\n🤖 aaf-bot: unterminated\n"),
            None
        );
    }

    #[test]
    fn lexer_preserves_cat_heredoc_only_for_pure_dollar_subst() {
        let script = shell::parse("gh pr create --body \"$(cat <<'EOF'\n🤖 aaf-bot: x\nEOF\n)\"");
        let cmd = script.cmds.last().expect("cmd");
        assert_eq!(cmd.arg_dynamic.get(4), Some(&true));
        assert_eq!(
            cmd.arg_cat_heredoc.get(4).and_then(Option::as_deref),
            Some("🤖 aaf-bot: x\n")
        );

        let unquoted = shell::parse("gh pr create --body $(cat <<'EOF'\n🤖 aaf-bot: x\nEOF\n)");
        let cmd = unquoted.cmds.last().expect("cmd");
        assert_eq!(
            cmd.arg_cat_heredoc.get(4).and_then(Option::as_deref),
            Some("🤖 aaf-bot: x\n")
        );

        for rejected in [
            "gh pr create --body \"`cat <<'EOF'\n🤖 aaf-bot: x\nEOF\n`\"",
            "gh pr create --body \"$(cat <<'EOF'\n🤖 aaf-bot: x\nEOF\n)x\"",
            "gh pr create --body \"x$(cat <<'EOF'\n🤖 aaf-bot: x\nEOF\n)\"",
            "gh pr create --body $(cat <<'EOF'\n🤖 aaf-bot: x\nEOF\n)x",
            "gh pr create --body x$(cat <<'EOF'\n🤖 aaf-bot: x\nEOF\n)",
            "gh pr create --body \"$(cat <<'EOF'\n🤖 aaf-bot: x\nEOF\n)$(echo y)\"",
        ] {
            let s = shell::parse(rejected);
            let c = s.cmds.last().expect("cmd");
            assert_eq!(
                c.arg_cat_heredoc.get(4).and_then(Option::as_deref),
                None,
                "expected None for {rejected}"
            );
        }
    }
}
