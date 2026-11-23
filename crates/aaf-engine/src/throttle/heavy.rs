//! Which commands start a build tool that can freeze the machine. One
//! classifier for the gate (such a command runs alone), for `aaf slot` and
//! for the doctor's look at the git hooks, so the three cannot drift.
//!
//! The text is read the way a shell would split it, without running it: a
//! program counts when it stands where a command starts. Quotes are dropped
//! and every `;`, `&`, `|`, parenthesis, brace, backtick or newline starts a
//! new command, also inside quotes, so `sh -c 'cd app && ./gradlew test'` is
//! found. That errs on the side of "heavy": `echo "a; gradle"` and
//! `git checkout -- gradlew` are classified too. An argument is not:
//! `echo gradle`, `ls gradle/`, `chmod +x gradlew`.

use std::ffi::OsStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeavyTool {
    Gradle,
    Xcodebuild,
    Unity,
}

impl HeavyTool {
    pub fn name(self) -> &'static str {
        match self {
            HeavyTool::Gradle => "gradle",
            HeavyTool::Xcodebuild => "xcodebuild",
            HeavyTool::Unity => "unity",
        }
    }
}

/// Programs, by the last component of their path, whatever the case and
/// whatever their arguments: `gradle --version` runs alone too. The hook's
/// guard (`aaf-hook`, which cannot depend on this crate) lets such
/// information calls through; this table must stay a superset of what the
/// hook calls heavy, never the other way round.
pub const HEAVY_PROGRAMS: &[(&str, HeavyTool)] = &[
    ("gradle", HeavyTool::Gradle),
    ("gradlew", HeavyTool::Gradle),
    ("gradlew.bat", HeavyTool::Gradle),
    ("xcodebuild", HeavyTool::Xcodebuild),
    ("unity", HeavyTool::Unity),
    ("unity.exe", HeavyTool::Unity),
    ("unity-editor", HeavyTool::Unity),
];

/// Arguments that give the tool away wherever they stand, whatever the
/// case: the Unity editor is started under many names and through
/// scripts, and always with one of these when it does heavy work.
pub const HEAVY_FLAGS: &[(&str, HeavyTool)] = &[
    ("-batchmode", HeavyTool::Unity),
    ("-runTests", HeavyTool::Unity),
    ("-executeMethod", HeavyTool::Unity),
];

/// Common build/test tool families used ONLY to share learned peak-RSS
/// measurements across custom `--label` runs in the same repository (e.g.
/// `aaf slot --label fix-g6 -- cargo test ...` reusing `@cmd:cargo` instead of
/// falling back to `Admission::Unmeasured` and triggering a machine-wide drain).
/// Separate from [`HEAVY_PROGRAMS`] so daemon-exclusive policy and `aaf-hook`
/// rules remain unchanged.
const STATS_FAMILIES: &[(&str, &str)] = &[
    ("cargo", "cargo"),
    ("rustc", "cargo"),
    ("gradle", "gradle"),
    ("gradlew", "gradle"),
    ("gradlew.bat", "gradle"),
    ("xcodebuild", "xcodebuild"),
    ("unity", "unity"),
    ("unity.exe", "unity"),
    ("unity-editor", "unity"),
    ("mvn", "maven"),
    ("mvnw", "maven"),
    ("flutter", "flutter"),
    ("dart", "flutter"),
    ("npm", "node"),
    ("npx", "node"),
    ("pnpm", "node"),
    ("yarn", "node"),
    ("bun", "node"),
    ("pytest", "python"),
    ("uv", "python"),
    ("go", "go"),
    ("swift", "swift"),
    ("bazel", "bazel"),
    ("bazelisk", "bazel"),
    ("make", "make"),
    ("cmake", "make"),
    ("ninja", "make"),
    ("dotnet", "dotnet"),
    ("sbt", "sbt"),
    ("zig", "zig"),
];

/// Words after which the real program is still to come. Their own options
/// (words starting with `-` or a digit) are skipped as well, and so is the
/// value of an option that takes one ([`valued_options`]).
pub const WRAPPERS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "dash",
    "env",
    "exec",
    "command",
    "builtin",
    "noglob",
    "nice",
    "nohup",
    "setsid",
    "unbuffer",
    "script",
    "time",
    "timeout",
    "sudo",
    "doas",
    "stdbuf",
    "ionice",
    "xargs",
    "xcrun",
    "arch",
    "caffeinate",
    "taskpolicy",
    "if",
    "then",
    "else",
    "elif",
    "while",
    "until",
    "do",
    "!",
];

/// Wrappers that run a script given after `-c`.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash"];

/// Options of a wrapper that take a separate value: the single-letter ones,
/// then those matched as a whole word.
type Valued = (&'static str, &'static [&'static str]);

const NO_VALUED: Valued = ("", &[]);

/// Which options of `wrapper` are followed by a value. Missing one would
/// make its value the command and hide the real one behind it
/// (`env -u FOO gradle build`). The rows for the wrappers the hook knows
/// are those of `wrapper_len` in `aaf-hook/src/shell/command.rs` (which
/// cannot be shared: the hook does not depend on this crate); keep the two
/// in step.
fn valued_options(wrapper: &str) -> Valued {
    match wrapper {
        "exec" => ("a", &[]),
        "sudo" | "doas" => (
            "ugpChDRTU",
            &[
                "--user",
                "--group",
                "--prompt",
                "--close-from",
                "--chdir",
                "--chroot",
                "--host",
                "--role",
                "--type",
                "--other-user",
                "--command-timeout",
            ],
        ),
        "env" => ("uCP", &["--unset", "--chdir"]),
        // macOS `-t time`, `-T fmt`; util-linux the rest. macOS `-F` is a
        // flag: `script -F <pipe> cmd` names the file, not a value of `-F`.
        "script" => (
            "tTIOBmEo",
            &[
                "--log-in",
                "--log-out",
                "--log-io",
                "--log-timing",
                "--logging-format",
                "--echo",
                "--output-limit",
            ],
        ),
        "nice" => ("n", &["--adjustment"]),
        "timeout" => ("sk", &["--signal", "--kill-after"]),
        "stdbuf" => ("ioe", &["--input", "--output", "--error"]),
        "ionice" => ("cn", &["--class", "--classdata"]),
        "caffeinate" => ("tw", &[]),
        "xargs" => (
            "nIPLdEsJRa",
            &[
                "--max-args",
                "--max-procs",
                "--max-chars",
                "--delimiter",
                "--arg-file",
                "--process-slot-var",
            ],
        ),
        // Not wrappers for the hook; the options are from their man pages.
        "arch" => ("de", &["-arch"]),
        "xcrun" => ("", &["--sdk", "-sdk", "--toolchain", "-toolchain"]),
        "taskpolicy" => ("cdgtl", &[]),
        // `-o pipefail`, `+O extglob` (a `+` option is read like a `-` one).
        "sh" | "bash" | "zsh" | "dash" => ("oO", &["--rcfile", "--init-file"]),
        _ => NO_VALUED,
    }
}

/// Wrappers whose single-dash options are whole words, never clusters of
/// letters: `arch -arm64e` names an architecture, it is not `-a -r -m …
/// -e <value>`.
const NO_CLUSTERS: &[&str] = &["arch", "xcrun"];

fn takes_value(wrapper: &str, (letters, words): Valued, option: &str) -> bool {
    if words.contains(&option) {
        return true;
    }
    if option.starts_with("--") {
        return false;
    }
    let Some(rest) = option.strip_prefix(['-', '+']) else {
        return false;
    };
    if NO_CLUSTERS.contains(&wrapper) {
        return rest.len() == 1 && letters.contains(rest);
    }
    // In a short cluster (`-iu FOO`, `-iP /dir`), the first valued letter takes
    // the rest of the word if glued, or the next word if it ends the cluster.
    rest.char_indices()
        .find(|(_, c)| letters.contains(*c))
        .is_some_and(|(at, c)| at + c.len_utf8() == rest.len())
}

/// `-c`, also bundled with other letters (`-lc`).
fn is_c_flag(option: &str) -> bool {
    option.starts_with('-') && !option.starts_with("--") && option.contains('c')
}

/// A shell option word: `-x`, `+o`, `--norc`. A lone `-` or `--` ends the
/// options instead.
fn is_shell_option(word: &str) -> bool {
    word.len() > 1 && word.starts_with(['-', '+']) && word != "--"
}

/// The script a shell runs for `-c`: the first word after all its options,
/// not the word right after `-c` (`sh -c -- 'cmd'`, `bash -c -o pipefail
/// 'cmd'`, `zsh -c +o nomatch 'cmd'`). `flag` is the word that held the
/// `c`; it may hold an `o` whose value comes next (`-co pipefail`).
fn shell_script<'a>(
    flag: &str,
    words: &mut impl Iterator<Item = std::borrow::Cow<'a, str>>,
) -> Option<std::borrow::Cow<'a, str>> {
    let shell = valued_options("sh");
    let mut skip = takes_value("sh", shell, flag);
    while let Some(word) = words.next() {
        if std::mem::take(&mut skip) {
            continue;
        }
        if word == "--" || word == "-" {
            return words.next();
        }
        if !is_shell_option(&word) {
            return Some(word);
        }
        skip = takes_value("sh", shell, &word);
    }
    None
}

/// Option of `env` (`-S`, `-iS`, `-S<cmd>`, `--split-string[=<cmd>]`) or
/// `script` (`-c`, `-qc`, `-c<cmd>`, `--command[=<cmd>]`) that carries a
/// command string: `Some("")` when the command is the next word, `Some(glued)`
/// when glued to the option.
fn carried_script<'a>(wrapper: &str, option: &'a str) -> Option<&'a str> {
    let (long, short) = match wrapper {
        "env" => ("--split-string", 'S'),
        "script" => ("--command", 'c'),
        _ => return None,
    };
    if let Some(after) = option.strip_prefix(long) {
        return match after.strip_prefix('=') {
            Some(glued) => Some(glued),
            None => after.is_empty().then_some(""),
        };
    }
    if !option.starts_with('-') || option.starts_with("--") {
        return None;
    }
    // The script letter counts only before any letter that takes a value
    // (in `-uS`, `S` is the name `-u` unsets).
    let (letters, _) = valued_options(wrapper);
    let at = option.find(|c: char| c == short || letters.contains(c))?;
    option[at..].strip_prefix(short)
}

enum Token {
    Word(String),
    /// A new command starts here.
    Break,
}

fn tokens(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut chars = text.chars();
    let flush = |word: &mut String, out: &mut Vec<Token>| {
        if !word.is_empty() {
            out.push(Token::Word(std::mem::take(word)));
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                // The next character is taken as it is; a line continuation
                // is plain white space.
                match chars.next() {
                    Some('\n') | None => flush(&mut word, &mut out),
                    Some(next) => word.push(next),
                }
            }
            '\'' | '"' => match quote {
                Some(open) if open == c => quote = None,
                Some(_) => word.push(c),
                None => quote = Some(c),
            },
            '#' if quote.is_none() && word.is_empty() => {
                // A comment runs to the end of the line.
                for rest in chars.by_ref() {
                    if rest == '\n' {
                        break;
                    }
                }
                out.push(Token::Break);
            }
            ';' | '&' | '|' | '(' | ')' | '{' | '}' | '`' | '\n' => {
                flush(&mut word, &mut out);
                out.push(Token::Break);
            }
            c if c.is_whitespace() => flush(&mut word, &mut out),
            c => word.push(c),
        }
    }
    flush(&mut word, &mut out);
    out
}

/// `NAME=value` in front of a command.
fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn lookup(table: &[(&str, HeavyTool)], word: &str) -> Option<HeavyTool> {
    table
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(word))
        .map(|(_, tool)| *tool)
}

fn lookup_family(word: &str) -> Option<&'static str> {
    STATS_FAMILIES
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(word))
        .map(|(_, family)| *family)
}

/// What one word did to the scan.
enum Step<'a> {
    Heavy(HeavyTool),
    /// A known build/test command family for stats sharing (not a daemon-heavy tool).
    Family(&'static str),
    /// The word was a shell's `-c` or a wrapper's `-S`/`-c`: the script is the next argument.
    Script,
    /// A script glued to `-S<cmd>`, `--split-string=<cmd>`, `-c<cmd>`, or `--command=<cmd>`.
    InlineScript(&'a str),
    Next,
}

/// Reads one command after the other, a word at a time.
struct Scanner {
    /// A command given to `aaf slot` or `aaf gate` does not count.
    skip_queued: bool,
    at_command: bool,
    in_wrapper: bool,
    /// Active wrapper name (for wrapper-specific script options).
    wrapper: &'static str,
    /// The wrapper last seen is a shell.
    shell: bool,
    /// `script [opts] <typescript-file> [command]`: skip the recording file once.
    skip_file: bool,
    valued: Valued,
    /// The next word is the value of a wrapper's option.
    value_next: bool,
    after_aaf: bool,
    queued: bool,
}

impl Scanner {
    fn new(skip_queued: bool) -> Scanner {
        Scanner {
            skip_queued,
            at_command: true,
            in_wrapper: false,
            wrapper: "",
            shell: false,
            skip_file: false,
            valued: NO_VALUED,
            value_next: false,
            after_aaf: false,
            queued: false,
        }
    }

    /// A new command starts with the next word.
    fn start_command(&mut self) {
        *self = Scanner::new(self.skip_queued);
    }

    /// Only the heavy flags count from here on: what follows are arguments.
    fn arguments_only(&mut self) {
        self.at_command = false;
        self.in_wrapper = false;
        self.wrapper = "";
        self.skip_file = false;
    }

    fn word<'a>(&mut self, word: &'a str) -> Step<'a> {
        if self.queued {
            return Step::Next;
        }
        if std::mem::take(&mut self.after_aaf)
            && self.skip_queued
            && matches!(word, "slot" | "gate")
        {
            // The rest of this command already goes through the queue.
            self.queued = true;
            return Step::Next;
        }
        if let Some(tool) = lookup(HEAVY_FLAGS, word) {
            return Step::Heavy(tool);
        }
        if word == "--" {
            // What follows is usually the program a wrapper runs.
            self.at_command = true;
            self.in_wrapper = true;
            self.wrapper = "";
            self.shell = false;
            self.skip_file = false;
            self.valued = NO_VALUED;
            self.value_next = false;
            return Step::Next;
        }
        if !self.at_command || std::mem::take(&mut self.value_next) || is_assignment(word) {
            return Step::Next;
        }
        let base = basename(word);
        if let Some(&known) = WRAPPERS.iter().find(|&&w| w == base) {
            self.in_wrapper = true;
            self.wrapper = known;
            self.shell = SHELLS.contains(&known);
            self.skip_file = known == "script";
            self.valued = valued_options(known);
            return Step::Next;
        }
        if self.in_wrapper && (word.starts_with('-') || (self.shell && is_shell_option(word))) {
            if self.shell && is_c_flag(word) {
                // In text the words that follow are read on: an `o` in the
                // same word (`-co pipefail`) still takes the next one.
                self.value_next = takes_value(self.wrapper, self.valued, word);
                return Step::Script;
            }
            if let Some(glued) = carried_script(self.wrapper, word) {
                self.skip_file = false;
                return if glued.is_empty() {
                    Step::Script
                } else {
                    Step::InlineScript(glued)
                };
            }
            self.value_next = takes_value(self.wrapper, self.valued, word);
            return Step::Next;
        }
        if self.in_wrapper && std::mem::take(&mut self.skip_file) {
            return Step::Next;
        }
        if self.in_wrapper && word.starts_with(|c: char| c.is_ascii_digit()) {
            return Step::Next;
        }
        self.at_command = false;
        if let Some(tool) = lookup(HEAVY_PROGRAMS, base) {
            return Step::Heavy(tool);
        }
        if let Some(family) = lookup_family(base) {
            return Step::Family(family);
        }
        self.after_aaf = base == "aaf";
        Step::Next
    }
}

fn scan(text: &str, skip_queued: bool) -> Option<HeavyTool> {
    let mut scanner = Scanner::new(skip_queued);
    for token in tokens(text) {
        match token {
            Token::Break => scanner.start_command(),
            Token::Word(word) => match scanner.word(&word) {
                Step::Heavy(tool) => return Some(tool),
                Step::InlineScript(script) => {
                    if let Some(tool) = scan(script, skip_queued) {
                        return Some(tool);
                    }
                }
                // In text the script after `-c`/`-S` is tokenised with the rest:
                // its first word is read as the command the wrapper runs.
                Step::Family(_) | Step::Script | Step::Next => {}
            },
        }
    }
    None
}

/// The heavy build tool a shell command starts, if any.
pub fn heavy_tool(command: &str) -> Option<HeavyTool> {
    scan(command, false)
}

/// Like [`heavy_tool`], but a command given to `aaf slot` or `aaf gate` does
/// not count: it already waits its turn. For text that runs outside `aaf`
/// (a git hook).
pub fn unqueued_heavy_tool(command: &str) -> Option<HeavyTool> {
    scan(command, true)
}

/// [`heavy_tool`] for an argument vector (`aaf slot -- ./gradlew test`,
/// `sh -c "…"`). Every element is one word, whatever it contains: no shell
/// splits `/tmp/my app/gradlew` at the space. Only the script a shell is
/// handed after `-c` is shell text and read like a command line.
pub fn heavy_tool_argv<S: AsRef<OsStr>>(argv: &[S]) -> Option<HeavyTool> {
    let mut scanner = Scanner::new(false);
    let mut words = argv.iter().map(|arg| arg.as_ref().to_string_lossy());
    while let Some(word) = words.next() {
        match scanner.word(&word) {
            Step::Heavy(tool) => return Some(tool),
            Step::Script => {
                let script = if scanner.shell {
                    shell_script(&word, &mut words)
                } else {
                    // `env -S`, `script -c`: the string is the next word.
                    words.next()
                };
                if let Some(tool) = script.and_then(|script| heavy_tool(&script)) {
                    return Some(tool);
                }
                // The rest are the script's `$0`, `$1`, …
                scanner.arguments_only();
            }
            Step::InlineScript(script) => {
                if let Some(tool) = heavy_tool(script) {
                    return Some(tool);
                }
                scanner.arguments_only();
            }
            Step::Family(_) | Step::Next => {}
        }
    }
    None
}

/// Build/test command family for sharing peak-RSS measurements across custom
/// `--label` invocations in the same repository.
pub fn stats_command_family(command: &str) -> Option<&'static str> {
    let mut scanner = Scanner::new(false);
    for token in tokens(command) {
        match token {
            Token::Break => scanner.start_command(),
            Token::Word(word) => match scanner.word(&word) {
                Step::Heavy(tool) => return Some(tool.name()),
                Step::Family(family) => return Some(family),
                Step::InlineScript(script) => {
                    if let Some(family) = stats_command_family(script) {
                        return Some(family);
                    }
                }
                Step::Script | Step::Next => {}
            },
        }
    }
    None
}

/// [`stats_command_family`] for an argument vector (`aaf slot --label x -- sh -c 'cargo test'`).
pub fn stats_command_family_argv<S: AsRef<OsStr>>(argv: &[S]) -> Option<&'static str> {
    let mut scanner = Scanner::new(false);
    let mut words = argv.iter().map(|arg| arg.as_ref().to_string_lossy());
    while let Some(word) = words.next() {
        match scanner.word(&word) {
            Step::Heavy(tool) => return Some(tool.name()),
            Step::Family(family) => return Some(family),
            Step::Script => {
                let script = if scanner.shell {
                    shell_script(&word, &mut words)
                } else {
                    words.next()
                };
                if let Some(family) = script.and_then(|script| stats_command_family(&script)) {
                    return Some(family);
                }
                scanner.arguments_only();
            }
            Step::InlineScript(script) => {
                if let Some(family) = stats_command_family(script) {
                    return Some(family);
                }
                scanner.arguments_only();
            }
            Step::Next => {}
        }
    }
    None
}
