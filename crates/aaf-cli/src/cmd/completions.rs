//! `aaf completions <shell>`: emit ready-to-eval shell completion scripts.

use std::fmt::Write as _;

use serde_json::json;

use crate::ctx::Ctx;
use crate::output::{CmdOutput, CmdResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
    Powershell,
}

impl ShellKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ShellKind::Bash => "bash",
            ShellKind::Zsh => "zsh",
            ShellKind::Fish => "fish",
            ShellKind::Powershell => "powershell",
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct CompletionsArgs {
    /// Target shell for the generated completion script.
    #[arg(value_enum)]
    pub shell: ShellKind,
}

/// Every top-level `aaf` subcommand exposed in completions.
pub const SUBCOMMANDS: &[(&str, &str)] = &[
    ("init", "Detect stack, write config, register hook"),
    ("doctor", "Check installation on machine and repo"),
    ("gate", "Run gate and write signed receipt"),
    ("merge", "Verify and merge PR under merge lock"),
    ("approve", "Sign approval for head-locked files"),
    ("board", "Manage task board, dependencies, and freeze"),
    ("cockpit", "Interactive TTY cockpit for tasks"),
    ("run", "Run ready tasks through agent and gate"),
    ("slot", "Run command through heavy-build queue"),
    ("status", "Read-only cost, token, and task overview"),
    ("plan", "Build read-only planning prompt"),
    ("proof", "Record, verify, or show device proofs"),
    ("skills", "List or sync bundled domain skills"),
    ("watch", "Live read-only terminal dashboard"),
    ("telegram", "Run outbound Telegram remote companion"),
    ("sop", "List or execute standard operating procedures"),
    ("executors", "Inspect configured agent executors"),
    ("ui", "Start local web UI dashboard and mobile QR"),
    ("upgrade", "Self-update aaf with SHA-256 verification"),
    ("completions", "Generate shell completion scripts"),
    ("gc", "Clean orphaned worktrees, refs, and streams"),
    ("handover", "Build read-only acceptance handover report"),
    ("accounts", "List accounts, pick next, manage cooldowns"),
];

pub fn run(args: &CompletionsArgs, _ctx: &Ctx) -> CmdResult {
    let script = render_script(args.shell);
    Ok(CmdOutput::ok("ok", script.clone())
        .field("shell", json!(args.shell.as_str()))
        .field("script", json!(script)))
}

/// Generate a standalone completion script for `shell`.
pub fn render_script(shell: ShellKind) -> String {
    match shell {
        ShellKind::Bash => render_bash(),
        ShellKind::Zsh => render_zsh(),
        ShellKind::Fish => render_fish(),
        ShellKind::Powershell => render_powershell(),
    }
}

fn subcommand_names() -> String {
    SUBCOMMANDS
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_bash() -> String {
    let cmds = subcommand_names();
    format!(
        r#"# bash completion for aaf — evaluate via: eval "$(aaf completions bash)"
_aaf_completions() {{
    local cur prev cmd
    COMPREPLY=()
    cur="${{COMP_WORDS[COMP_CWORD]}}"
    prev="${{COMP_WORDS[COMP_CWORD-1]}}"

    local commands="{cmds}"
    local global_opts="--json --help --version"

    cmd=""
    local i
    for ((i = 1; i < COMP_CWORD; i++)); do
        case "${{COMP_WORDS[i]}}" in
            --json) ;;
            -*) ;;
            *)
                cmd="${{COMP_WORDS[i]}}"
                break
                ;;
        esac
    done

    if [[ -z "$cmd" ]]; then
        COMPREPLY=( $(compgen -W "$commands $global_opts" -- "$cur") )
        return 0
    fi

    case "$cmd" in
        init)
            COMPREPLY=( $(compgen -W "--force --with-skills --hook-host --host --json --help" -- "$cur") )
            ;;
        gate)
            COMPREPLY=( $(compgen -W "--scoped --full --release --json --help" -- "$cur") )
            ;;
        board)
            COMPREPLY=( $(compgen -W "add list show unblock unfreeze rework trace split dep graph approve-plan undo steer import-issues sync-github --json --help" -- "$cur") )
            ;;
        accounts)
            COMPREPLY=( $(compgen -W "list next cooldown --json --help" -- "$cur") )
            ;;
        proof)
            COMPREPLY=( $(compgen -W "record verify show --json --help" -- "$cur") )
            ;;
        skills)
            COMPREPLY=( $(compgen -W "list sync --json --help" -- "$cur") )
            ;;
        sop)
            COMPREPLY=( $(compgen -W "list run show --json --help" -- "$cur") )
            ;;
        executors)
            COMPREPLY=( $(compgen -W "list check --json --help" -- "$cur") )
            ;;
        ui)
            COMPREPLY=( $(compgen -W "--port --bind --qr --token --json --help" -- "$cur") )
            ;;
        telegram)
            COMPREPLY=( $(compgen -W "--once --all --repo --send-test --json --help" -- "$cur") )
            ;;
        upgrade)
            COMPREPLY=( $(compgen -W "--check --version --dry-run --dir --repo --json --help" -- "$cur") )
            ;;
        completions)
            COMPREPLY=( $(compgen -W "bash zsh fish powershell --json --help" -- "$cur") )
            ;;
        run)
            COMPREPLY=( $(compgen -W "--auto --auto-merge --auto-split --max-parallel --unsandboxed --json --help" -- "$cur") )
            ;;
        *)
            COMPREPLY=( $(compgen -W "--json --help" -- "$cur") )
            ;;
    esac
}}

complete -F _aaf_completions aaf
"#
    )
}

fn render_zsh() -> String {
    let mut cmd_lines = String::new();
    for (name, desc) in SUBCOMMANDS {
        let _ = writeln!(cmd_lines, "        '{name}:{desc}'");
    }
    format!(
        r#"#compdef aaf
# zsh completion for aaf — evaluate via: eval "$(aaf completions zsh)"

_aaf() {{
    local -a commands
    commands=(
{cmd_lines}    )

    _arguments -C \
        '--json[Print exactly one JSON object on stdout]' \
        '(-h --help)'{{-h,--help}}'[Show help]' \
        '(-V --version)'{{-V,--version}}'[Show version]' \
        '1:command:->cmd' \
        '*::arg:->args'

    case "$state" in
        cmd)
            _describe -t commands 'aaf commands' commands
            ;;
        args)
            case "${{words[1]}}" in
                init)
                    _arguments '--force' '--with-skills' '--hook-host[Hook host]:host:(claude codex gemini all)' '--json'
                    ;;
                gate)
                    _arguments '--scoped' '--full' '--release' '--json'
                    ;;
                board)
                    _values 'board subcommand' add list show unblock unfreeze rework trace split dep graph approve-plan undo steer import-issues sync-github
                    ;;
                accounts)
                    _values 'accounts subcommand' list next cooldown
                    ;;
                proof)
                    _values 'proof subcommand' record verify show
                    ;;
                skills)
                    _values 'skills subcommand' list sync
                    ;;
                sop)
                    _values 'sop subcommand' list run show
                    ;;
                executors)
                    _values 'executors subcommand' list check
                    ;;
                ui)
                    _arguments '--port[Port]:port:' '--bind[Bind address]:addr:' '--qr[Show QR code]' '--json'
                    ;;
                telegram)
                    _arguments '--once' '--all' '--repo[Repo path]:dir:_files -/' '--send-test' '--json'
                    ;;
                upgrade)
                    _arguments '--check' '--version[Release tag]:tag:' '--dry-run' '--dir[Install dir]:dir:_files -/' '--repo[GitHub repo]:repo:' '--json'
                    ;;
                completions)
                    _values 'shell' bash zsh fish powershell
                    ;;
                *)
                    _arguments '--json' '--help'
                    ;;
            esac
            ;;
    esac
}}

if [ "$funcstack[1]" = "_aaf" ]; then
    _aaf "$@"
else
    compdef _aaf aaf
fi
"#
    )
}

fn render_fish() -> String {
    let mut out = String::from(
        "# fish completion for aaf — evaluate via: aaf completions fish | source\n\
         complete -c aaf -f\n\
         complete -c aaf -l json -d 'Print exactly one JSON object on stdout'\n\
         complete -c aaf -s h -l help -d 'Show help'\n\
         complete -c aaf -s V -l version -d 'Show version'\n",
    );
    for (name, desc) in SUBCOMMANDS {
        let _ = writeln!(
            out,
            "complete -c aaf -n '__fish_use_subcommand' -a '{name}' -d '{desc}'"
        );
    }
    out.push_str(
        r#"complete -c aaf -n '__fish_seen_subcommand_from init' -l force -d 'Overwrite existing config'
complete -c aaf -n '__fish_seen_subcommand_from init' -l with-skills -d 'Sync bundled skills'
complete -c aaf -n '__fish_seen_subcommand_from init' -l hook-host -xa 'claude codex gemini all' -d 'Hook host adapter'
complete -c aaf -n '__fish_seen_subcommand_from gate' -l scoped -d 'Run scoped gate'
complete -c aaf -n '__fish_seen_subcommand_from gate' -l full -d 'Run full gate'
complete -c aaf -n '__fish_seen_subcommand_from gate' -l release -d 'Run release gate'
complete -c aaf -n '__fish_seen_subcommand_from board' -a 'add list show unblock unfreeze rework trace split dep graph approve-plan undo steer import-issues sync-github'
complete -c aaf -n '__fish_seen_subcommand_from accounts' -a 'list next cooldown'
complete -c aaf -n '__fish_seen_subcommand_from proof' -a 'record verify show'
complete -c aaf -n '__fish_seen_subcommand_from skills' -a 'list sync'
complete -c aaf -n '__fish_seen_subcommand_from sop' -a 'list run show'
complete -c aaf -n '__fish_seen_subcommand_from executors' -a 'list check'
complete -c aaf -n '__fish_seen_subcommand_from ui' -l qr -d 'Print mobile QR pairing code'
complete -c aaf -n '__fish_seen_subcommand_from telegram' -l once -d 'Run one poll cycle'
complete -c aaf -n '__fish_seen_subcommand_from upgrade' -l check -d 'Check for updates only'
complete -c aaf -n '__fish_seen_subcommand_from upgrade' -l version -d 'Specific release tag'
complete -c aaf -n '__fish_seen_subcommand_from upgrade' -l dry-run -d 'Download and verify SHA-256 only'
complete -c aaf -n '__fish_seen_subcommand_from upgrade' -l dir -r -d 'Destination directory'
complete -c aaf -n '__fish_seen_subcommand_from upgrade' -l repo -r -d 'GitHub repository slug'
complete -c aaf -n '__fish_seen_subcommand_from completions' -a 'bash zsh fish powershell' -d 'Shell kind'
"#,
    );
    out
}

fn render_powershell() -> String {
    let mut entries = String::new();
    for (name, desc) in SUBCOMMANDS {
        let _ = writeln!(entries, "        @{{ Name = '{name}'; Desc = '{desc}' }}");
    }
    format!(
        r#"# PowerShell completion for aaf — evaluate via: aaf completions powershell | Out-String | Invoke-Expression
Register-ArgumentCompleter -Native -CommandName aaf -ScriptBlock {{
    param($wordToComplete, $commandAst, $cursorPosition)

    $commands = @(
{entries}    )
    $globalFlags = @('--json', '--help', '--version')

    $tokens = @($commandAst.CommandElements | ForEach-Object {{ $_.Extent.Text }})
    $sub = $null
    for ($i = 1; $i -lt $tokens.Count; $i++) {{
        if ($tokens[$i] -notlike '-*') {{
            $sub = $tokens[$i]
            break
        }}
    }}

    if (-not $sub -or ($tokens.Count -eq 2 -and $wordToComplete -eq $sub)) {{
        $commands | Where-Object {{ $_.Name -like "$wordToComplete*" }} | ForEach-Object {{
            [System.Management.Automation.CompletionResult]::new($_.Name, $_.Name, 'ParameterValue', $_.Desc)
        }}
        $globalFlags | Where-Object {{ $_ -like "$wordToComplete*" }} | ForEach-Object {{
            [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterName', $_)
        }}
        return
    }}

    $subOpts = switch ($sub) {{
        'init'        {{ @('--force', '--with-skills', '--hook-host', '--json', '--help') }}
        'gate'        {{ @('--scoped', '--full', '--release', '--json', '--help') }}
        'board'       {{ @('add', 'list', 'show', 'unblock', 'unfreeze', 'rework', 'trace', 'split', 'dep', 'graph', 'approve-plan', 'undo', 'steer', 'import-issues', 'sync-github', '--json') }}
        'accounts'    {{ @('list', 'next', 'cooldown', '--json') }}
        'proof'       {{ @('record', 'verify', 'show', '--json') }}
        'skills'      {{ @('list', 'sync', '--json') }}
        'sop'         {{ @('list', 'run', 'show', '--json') }}
        'executors'   {{ @('list', 'check', '--json') }}
        'ui'          {{ @('--port', '--bind', '--qr', '--json') }}
        'telegram'    {{ @('--once', '--all', '--repo', '--send-test', '--json') }}
        'upgrade'     {{ @('--check', '--version', '--dry-run', '--dir', '--repo', '--json') }}
        'completions' {{ @('bash', 'zsh', 'fish', 'powershell', '--json') }}
        default       {{ @('--json', '--help') }}
    }}

    $subOpts | Where-Object {{ $_ -like "$wordToComplete*" }} | ForEach-Object {{
        [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)
    }}
}}
"#
    )
}
