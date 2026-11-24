//! Per-subcommand flag tables (verified against `gh 2.103.3 --help`) and
//! pflag-compatible short-cluster / glued-value parsing for `gh pr|issue`
//! write subcommands.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteSub {
    PrComment,
    PrCreate,
    PrEdit,
    PrReview,
    PrClose,
    PrReopen,
    IssueComment,
    IssueCreate,
    IssueEdit,
    IssueClose,
    IssueReopen,
}

impl WriteSub {
    pub(super) fn parse(sub1: &str, sub2: &str) -> Option<Self> {
        match (sub1, sub2) {
            ("pr", "comment") => Some(Self::PrComment),
            ("pr", "create") => Some(Self::PrCreate),
            ("pr", "edit") => Some(Self::PrEdit),
            ("pr", "review") => Some(Self::PrReview),
            ("pr", "close") => Some(Self::PrClose),
            ("pr", "reopen") => Some(Self::PrReopen),
            ("issue", "comment") => Some(Self::IssueComment),
            ("issue", "create") => Some(Self::IssueCreate),
            ("issue", "edit") => Some(Self::IssueEdit),
            ("issue", "close") => Some(Self::IssueClose),
            ("issue", "reopen") => Some(Self::IssueReopen),
            _ => None,
        }
    }

    fn uses_comment_body(self) -> bool {
        matches!(
            self,
            Self::PrClose | Self::PrReopen | Self::IssueClose | Self::IssueReopen
        )
    }
}

struct FlagSpec {
    bool_short: &'static str,
    val_short: &'static str,
    bool_long: &'static [&'static str],
    val_long: &'static [&'static str],
}

fn spec_for(sub: WriteSub) -> FlagSpec {
    match sub {
        WriteSub::PrComment | WriteSub::IssueComment => FlagSpec {
            bool_short: "ewh",
            val_short: "bFR",
            bool_long: &[
                "create-if-none",
                "delete-last",
                "edit-last",
                "editor",
                "web",
                "yes",
                "help",
            ],
            val_long: &["attach", "body", "body-file", "repo"],
        },
        WriteSub::PrCreate => FlagSpec {
            bool_short: "defwh",
            val_short: "aBbFHlmpRTtr",
            bool_long: &[
                "draft",
                "dry-run",
                "editor",
                "fill",
                "fill-first",
                "fill-verbose",
                "no-maintainer-edit",
                "web",
                "help",
            ],
            val_long: &[
                "assignee",
                "attach",
                "base",
                "body",
                "body-file",
                "head",
                "label",
                "milestone",
                "project",
                "recover",
                "reviewer",
                "template",
                "title",
                "repo",
            ],
        },
        WriteSub::IssueCreate => FlagSpec {
            bool_short: "ewh",
            val_short: "abFlmpRTt",
            bool_long: &["editor", "web", "help"],
            val_long: &[
                "assignee",
                "attach",
                "blocked-by",
                "blocking",
                "body",
                "body-file",
                "label",
                "milestone",
                "parent",
                "project",
                "recover",
                "template",
                "title",
                "type",
                "repo",
            ],
        },
        WriteSub::PrEdit => FlagSpec {
            bool_short: "h",
            val_short: "BbFmRt",
            bool_long: &["remove-milestone", "help"],
            val_long: &[
                "add-assignee",
                "add-label",
                "add-project",
                "add-reviewer",
                "attach",
                "base",
                "body",
                "body-file",
                "milestone",
                "remove-assignee",
                "remove-label",
                "remove-project",
                "remove-reviewer",
                "title",
                "repo",
            ],
        },
        WriteSub::IssueEdit => FlagSpec {
            bool_short: "h",
            val_short: "bFmRt",
            bool_long: &["remove-milestone", "remove-parent", "remove-type", "help"],
            val_long: &[
                "add-assignee",
                "add-blocked-by",
                "add-blocking",
                "add-label",
                "add-project",
                "add-sub-issue",
                "attach",
                "body",
                "body-file",
                "milestone",
                "parent",
                "remove-assignee",
                "remove-blocked-by",
                "remove-blocking",
                "remove-label",
                "remove-project",
                "remove-sub-issue",
                "title",
                "type",
                "repo",
            ],
        },
        WriteSub::PrReview => FlagSpec {
            bool_short: "acrh",
            val_short: "bFR",
            bool_long: &["approve", "comment", "request-changes", "help"],
            val_long: &["body", "body-file", "repo"],
        },
        WriteSub::PrClose => FlagSpec {
            bool_short: "dh",
            val_short: "cR",
            bool_long: &["delete-branch", "help"],
            val_long: &["comment", "repo"],
        },
        WriteSub::PrReopen | WriteSub::IssueReopen => FlagSpec {
            bool_short: "h",
            val_short: "cR",
            bool_long: &["help"],
            val_long: &["comment", "repo"],
        },
        WriteSub::IssueClose => FlagSpec {
            bool_short: "h",
            val_short: "crR",
            bool_long: &["help"],
            val_long: &["comment", "duplicate-of", "reason", "repo"],
        },
    }
}

pub(super) enum BodySource {
    Text {
        value: String,
        val_idx: usize,
        is_glued: bool,
    },
    File {
        value: String,
        val_idx: usize,
    },
}

#[derive(Default)]
pub(super) struct ParsedWriteFlags {
    pub(super) editor: bool,
    pub(super) web: bool,
    pub(super) approve: bool,
    pub(super) delete_last: bool,
    pub(super) sources: Vec<BodySource>,
}

pub(super) fn parse_write_flags(
    sub: WriteSub,
    args: &[String],
) -> Result<ParsedWriteFlags, String> {
    let spec = spec_for(sub);
    let mut out = ParsedWriteFlags::default();
    let mut i = 0;

    while let Some(arg) = args.get(i).map(String::as_str) {
        if arg == "--" {
            break;
        }
        if arg == "-" || !arg.starts_with('-') {
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, glued) = long
                .split_once('=')
                .map_or((long, None), |(n, v)| (n, Some(v)));
            if spec.bool_long.contains(&name) {
                record_bool_long(name, &mut out);
                i += 1;
                continue;
            }
            if spec.val_long.contains(&name) {
                let (val, val_idx, is_glued, step) = match glued {
                    Some(v) => (v.to_string(), i, true, 1),
                    None => {
                        let Some(next) = args.get(i + 1) else {
                            return Err(format!("flag `--{name}` is missing its value"));
                        };
                        (next.clone(), i + 1, false, 2)
                    }
                };
                record_val_long(sub, name, val, val_idx, is_glued, &mut out);
                i += step;
                continue;
            }
            return Err(format!(
                "unknown flag `--{name}` on GitHub write subcommand cannot be verified safely"
            ));
        }

        // Short flag or pflag cluster (`-ew`, `-de`, `-btext`, `-F-`).
        let cluster = &arg[1..];
        let mut step = 1;
        for (offset, ch) in cluster.char_indices() {
            if spec.bool_short.contains(ch) {
                record_bool_short(sub, ch, &mut out);
                continue;
            }
            if spec.val_short.contains(ch) {
                let rem = &cluster[offset + ch.len_utf8()..];
                let (val, val_idx, is_glued) = if !rem.is_empty() {
                    (rem.to_string(), i, true)
                } else {
                    let Some(next) = args.get(i + 1) else {
                        return Err(format!("flag `-{ch}` is missing its value"));
                    };
                    step = 2;
                    (next.clone(), i + 1, false)
                };
                record_val_short(sub, ch, val, val_idx, is_glued, &mut out);
                break;
            }
            return Err(format!(
                "unknown flag `-{ch}` on GitHub write subcommand cannot be verified safely"
            ));
        }
        i += step;
    }

    Ok(out)
}

fn record_bool_long(name: &str, out: &mut ParsedWriteFlags) {
    match name {
        "editor" => out.editor = true,
        "web" => out.web = true,
        "approve" => out.approve = true,
        "delete-last" => out.delete_last = true,
        _ => {}
    }
}

fn record_bool_short(sub: WriteSub, ch: char, out: &mut ParsedWriteFlags) {
    match ch {
        'e' => out.editor = true,
        'w' => out.web = true,
        'a' if sub == WriteSub::PrReview => out.approve = true,
        _ => {}
    }
}

fn record_val_long(
    sub: WriteSub,
    name: &str,
    value: String,
    val_idx: usize,
    is_glued: bool,
    out: &mut ParsedWriteFlags,
) {
    if sub.uses_comment_body() {
        if name == "comment" {
            out.sources.push(BodySource::Text {
                value,
                val_idx,
                is_glued,
            });
        }
        return;
    }
    match name {
        "body" => out.sources.push(BodySource::Text {
            value,
            val_idx,
            is_glued,
        }),
        "body-file" => out.sources.push(BodySource::File { value, val_idx }),
        _ => {}
    }
}

fn record_val_short(
    sub: WriteSub,
    ch: char,
    value: String,
    val_idx: usize,
    is_glued: bool,
    out: &mut ParsedWriteFlags,
) {
    if sub.uses_comment_body() {
        if ch == 'c' {
            out.sources.push(BodySource::Text {
                value,
                val_idx,
                is_glued,
            });
        }
        return;
    }
    match ch {
        'b' => out.sources.push(BodySource::Text {
            value,
            val_idx,
            is_glued,
        }),
        'F' => out.sources.push(BodySource::File { value, val_idx }),
        _ => {}
    }
}
