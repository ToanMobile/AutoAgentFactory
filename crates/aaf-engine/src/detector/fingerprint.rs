//! Stable fingerprint of a stack trace.
//!
//! Two occurrences of the same bug rarely print the same bytes: line numbers
//! move with unrelated edits, and PIDs, addresses, timestamps and temp paths
//! change on every run. `normalize` masks those and keeps what identifies the
//! bug: exception types, messages and the top frame symbols in order.
//!
//! Pure and linear in the input size: every pattern is compiled once and runs
//! on the `regex` crate's linear-time engine, one pass per pattern per line.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Regex};

/// Frames kept per contiguous run of frames (one run per exception in a
/// "Caused by" chain). Deeper frames are entry-point plumbing whose depth
/// varies between occurrences.
pub const MAX_FRAMES_PER_RUN: usize = 20;

/// A pattern that does not compile is a programming error in this file; the
/// test suite forces every pattern, so this cannot fire in a released build.
fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|err| panic!("invalid built-in pattern {pattern:?}: {err}"))
}

const MONTH: &str = "(?:Jan(?:uary)?|Feb(?:ruary)?|Mar(?:ch)?|Apr(?:il)?|May|June?|July?|Aug(?:ust)?|Sep(?:t(?:ember)?)?|Oct(?:ober)?|Nov(?:ember)?|Dec(?:ember)?)";

/// Character that may precede an absolute path. Captured and re-emitted so
/// that `/srv/app/tmp/x` is not mistaken for the temp directory.
const PATH_START: &str = r#"(^|[\s"'`(=\[<:,])"#;

/// Masks applied to every line, in order. Earlier entries consume text that
/// later, broader entries would otherwise split (a UUID before hex ids,
/// timestamps and addresses before bare digit runs).
static MASKS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    let month_date = format!(
        r"\b(?:(?:Mon|Tue|Wed|Thu|Fri|Sat|Sun)[a-z]{{0,6}},?\s+)?(?:{MONTH}\.?\s+\d{{1,2}}\b(?:,?\s+\d{{4}}\b)?|\d{{1,2}}\s+{MONTH}\.?\s+\d{{4}}\b)"
    );
    let temp_path = format!(
        r#"{PATH_START}(?:(?:/private)?(?:/var/folders/[^/\s]+/[^/\s]+/T|/var/tmp|/tmp)/|[A-Za-z]:\\Users\\[^\\\s]+\\AppData\\Local\\Temp\\)[^/\\\s:"')]+"#
    );
    let home_path = format!(r#"{PATH_START}(?:(?:/Users|/home)/|[A-Za-z]:\\Users\\)[^/\\\s:"')]+"#);
    let specs: Vec<(String, &'static str)> = vec![
        (
            r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b"
                .into(),
            "<uuid>",
        ),
        (
            r"\b\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?".into(),
            "<ts>",
        ),
        (month_date, "<ts>"),
        (r"\b\d{4}[-/]\d{2}[-/]\d{2}\b".into(), "<ts>"),
        // Clock time, optionally with logcat's `MM-DD ` in front.
        (
            r"\b(?:\d{2}-\d{2}\s+)?\d{1,2}:\d{2}:\d{2}(?:[.,]\d+)?\b".into(),
            "<ts>",
        ),
        (temp_path, "${1}<tmp>"),
        (home_path, "${1}~"),
        // Rust legacy symbol hash: changes with every compiler or dependency bump.
        (r"::h[0-9a-f]{16}\b".into(), ""),
        (r"0[xX][0-9a-fA-F]+".into(), "<addr>"),
        // JVM identity hash, as in `Foo@4b1210ee`.
        (r"@[0-9a-f]{4,16}\b".into(), "@<hex>"),
    ];
    specs
        .into_iter()
        .map(|(pattern, replacement)| (compile(&pattern), replacement))
        .collect()
});

/// Candidate hex ids (commit hashes, object ids). Only replaced when the run
/// mixes digits and letters, so words like `defaced` and plain numbers are
/// left to the other rules.
static HEX_ID: LazyLock<Regex> = LazyLock::new(|| compile(r"\b[0-9a-fA-F]{7,}\b"));

/// Counter the compiler appends to anonymous classes and lambdas when it is
/// glued to the word (`ExternalSyntheticLambda3`). A `$1` style suffix starts
/// a new word and is masked by [`NUMBER`] instead.
static ANON_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| compile(r"(?i)(lambda|closure|anon|anonymous)\d+\b"));

/// A digit run at the start of a word. Digits inside an identifier
/// (`Http2Codec`, `Base64`) are part of the symbol and are kept.
static NUMBER: LazyLock<Regex> = LazyLock::new(|| compile(r"\b\d+"));

/// Counter suffix such as `thread_12` or `'invoice_4412'`.
static UNDERSCORE_NUMBER: LazyLock<Regex> = LazyLock::new(|| compile(r"_\d+\b"));

static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| compile(r"\s+"));

static ANSI: LazyLock<Regex> = LazyLock::new(|| compile(r"\x1b\[[0-9;?]*[A-Za-z]"));

/// logcat `threadtime` and `brief` prefixes: date, time, PID, TID, level, tag.
static LOG_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"^(?:\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2}\.\d+\s+\d+\s+\d+\s+[VDIWEFA]\s+[^:]*:\s?|[VDIWEFA]/[^(:]*\(\s*\d+\):\s?)",
    )
});

/// Lines that only count frames left out; the count follows the depth.
static ELIDED: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"^(?:\.\.\. \d+ (?:more|common frames omitted)|\[Previous line repeated \d+ more times?\])$",
    )
});

static PYTHON_FRAME: LazyLock<Regex> = LazyLock::new(|| compile(r#"^File "[^"]*", line \d+"#));

static RUST_FRAME: LazyLock<Regex> = LazyLock::new(|| compile(r"^\d+:\s+\S"));

/// JVM / JS `at …`, Apple crash report `3 Module 0x… symbol`, and
/// Gecko/WebKit `fn@file:line:col`.
static OTHER_FRAME: LazyLock<Regex> =
    LazyLock::new(|| compile(r"^(?:at\s+\S|\d+\s+\S.*\s0[xX][0-9a-fA-F]+\s|[^\s@]*@\S+:\d+:\d+$)"));

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Header,
    PythonFrame,
    RustFrame,
    Frame,
}

fn classify(line: &str) -> Kind {
    if PYTHON_FRAME.is_match(line) {
        Kind::PythonFrame
    } else if RUST_FRAME.is_match(line) {
        Kind::RustFrame
    } else if OTHER_FRAME.is_match(line) {
        Kind::Frame
    } else {
        Kind::Header
    }
}

fn mask(line: &str) -> String {
    let mut text = Cow::Borrowed(line);
    for (pattern, replacement) in MASKS.iter() {
        if let Cow::Owned(replaced) = pattern.replace_all(&text, *replacement) {
            text = Cow::Owned(replaced);
        }
    }
    let text = HEX_ID.replace_all(&text, |caps: &Captures<'_>| {
        let run = &caps[0];
        let mixed =
            run.bytes().any(|b| b.is_ascii_digit()) && run.bytes().any(|b| b.is_ascii_alphabetic());
        if mixed {
            "<hex>".to_string()
        } else {
            run.to_string()
        }
    });
    let text = ANON_SUFFIX.replace_all(&text, "${1}<n>");
    let text = NUMBER.replace_all(&text, "<n>");
    let text = UNDERSCORE_NUMBER.replace_all(&text, "_<n>");
    WHITESPACE.replace_all(text.trim(), " ").into_owned()
}

/// Normalised form of a trace: one masked line per kept input line.
///
/// An empty or whitespace-only trace normalises to the empty string; callers
/// must not fingerprint such input, or unrelated reports would collide.
pub fn normalize(trace: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut frames_in_run = 0usize;
    // Masked text of the last frame seen in the current run, kept or not.
    let mut last_frame: Option<String> = None;
    let mut last_kind = Kind::Header;
    let mut last_kept = false;

    for raw in trace.lines() {
        let line = ANSI.replace_all(raw, "");
        let line = LOG_PREFIX.replace(&line, "");
        let trimmed = line.trim();
        if trimmed.is_empty() || ELIDED.is_match(trimmed) {
            continue;
        }
        let indented = line.starts_with("    ") || line.starts_with('\t');
        if last_kind == Kind::PythonFrame && indented && !PYTHON_FRAME.is_match(trimmed) {
            // Source excerpt and caret markers under a Python frame: they
            // change with any edit to that line, the frame already names it.
            continue;
        }
        if last_kind == Kind::RustFrame && trimmed.starts_with("at ") {
            // Location line of the Rust frame above: one frame, not two.
            if last_kept && let Some(frame) = out.last_mut() {
                frame.push(' ');
                frame.push_str(&mask(trimmed));
            }
            last_kind = Kind::Header;
            continue;
        }

        let kind = classify(trimmed);
        let masked = mask(trimmed);
        if kind == Kind::Header {
            frames_in_run = 0;
            last_frame = None;
            last_kept = true;
            out.push(masked);
        } else {
            // Recursion repeats one frame a varying number of times.
            let repeated = last_frame.as_deref() == Some(masked.as_str());
            last_kept = !repeated && frames_in_run < MAX_FRAMES_PER_RUN;
            if last_kept {
                frames_in_run += 1;
                out.push(masked.clone());
            }
            last_frame = Some(masked);
        }
        last_kind = kind;
    }
    out.join("\n")
}

/// Lowercase sha256 hex of [`normalize`]'s output.
pub fn fingerprint(trace: &str) -> String {
    aaf_fp::sha256_hex(normalize(trace).as_bytes())
}
