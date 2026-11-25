# AutoAgentFactory (`aaf`) — 3-Minute Quickstart

Get from zero to a receipt-verified autonomous agent workflow on macOS, Linux, or Windows in under three minutes.

---

## 1. Install `aaf` and `aaf-hook`

Every release binary is verified byte-for-byte against `SHA256SUMS.txt` before extraction.
Until a release is published, the installer builds from source automatically when
`cargo` is installed (and tells you to install Rust from https://rustup.rs when it is not).

### macOS & Linux (1-line installer)

```sh
curl -fsSL https://raw.githubusercontent.com/ToanMobile/AutoAgentFactory/main/scripts/install.sh | sh
```

Pin a release or custom directory:

```sh
curl -fsSL https://raw.githubusercontent.com/ToanMobile/AutoAgentFactory/main/scripts/install.sh | sh -s -- --version v0.1.0 --dir ~/.local/bin
```

### Homebrew (macOS & Linux)

```sh
brew install --formula packaging/homebrew/aaf.rb
```

### Windows (PowerShell)

```powershell
irm https://raw.githubusercontent.com/ToanMobile/AutoAgentFactory/main/scripts/install.ps1 | iex
```

Pin a release or custom directory:

```powershell
.\scripts\install.ps1 -Version v0.1.0 -InstallDir "$env:LOCALAPPDATA\Programs\aaf\bin"
```

### Docker (hardened Linux runner with `bwrap`)

```sh
docker build -f packaging/docker/Dockerfile -t aaf:local .
docker run --rm -v "$PWD:/workspace" aaf:local doctor
```

Verify both binaries are on your `PATH`:

```sh
aaf --version
```

---

## 2. Enable Shell Completions

Enable tab-completion for all `aaf` subcommands and `--json`:

```sh
# Zsh (~/.zshrc)
eval "$(aaf completions zsh)"

# Bash (~/.bashrc)
eval "$(aaf completions bash)"

# Fish (~/.config/fish/config.fish)
aaf completions fish | source
```

```powershell
# PowerShell ($PROFILE)
aaf completions powershell | Out-String | Invoke-Expression
```

---

## 3. Set Up Your Repository (one command)

```sh
cd /path/to/your-repo
aaf setup
```

`aaf setup` detects which coding agents are installed (Claude Code, Codex CLI, Gemini CLI,
Antigravity, OpenCode), registers the `aaf-hook` for each, writes `.autofactory/config.toml`
for your stack (an existing one is kept), syncs the bundled skills, then runs every
`aaf doctor` check and lists what is left to fix. It is safe to re-run: running it again
is how you repair a hook that went missing.

Commit the generated repo config and hook registration so isolated task worktrees inherit them:

```sh
git add .autofactory/config.toml .claude/settings.json .gitignore
git commit -m "chore: initialize aaf"
```

Prefer the individual steps? `aaf init --with-skills --hook-host all` then `aaf doctor`
do the same thing.

---

## 4. Check Machine & Repo Health

```sh
aaf doctor
```

`aaf doctor` exits `0` when git, config, hook registration, `aaf-hook` binary, data directory, and HMAC receipt key are all healthy (or `2` if a required check fails). Add `--json` for machine-readable output. The same checks are one click away in Studio (section 6).

---

## 5. Run Built-In SOP Workflows

Inspect and run repeatable Standard Operating Procedures (SOPs):

```sh
aaf sop list
aaf sop run <sop-name>
```

Or queue and run individual tasks directly:

```sh
aaf board add \
  --title "Add empty state for orders screen" \
  --spec "Render an empty-state illustration when the user has zero orders." \
  --accept "Empty list renders empty-state view" \
  --priority P1

aaf run
```

---

## 6. Studio: every number on one page

Start the local web UI and open the **Studio** link it prints
(`http://127.0.0.1:4317/studio#token=…`; `aaf ui --open` opens it for you on macOS):

```sh
aaf ui          # add --qr to pair a phone on the local network
```

Studio shows, for the last 24 hours, 7 days, 30 days or all history: tasks done,
autonomy and first-pass gate rate, estimated cost per executor, lead time, time per
lifecycle stage (p50/p90), why tasks are blocked, each task's acceptance criteria,
live runs and accounts, and the `aaf doctor` checks on one click. It refreshes every
15 seconds. The same report is available in the terminal as `aaf report [--since HOURS]`.

---

## 7. Remote Telegram Companion (Zero Open Ports)

Run the outbound-only Telegram long-polling companion to inspect `/status`, `/board`, `/handover`, and issue two-step `/confirm` approvals from your phone:

```sh
export AAF_TELEGRAM_BOT_TOKEN="<your-bot-token>"
aaf telegram --send-test
aaf telegram
```

Configure `allowed_chat_ids` in `~/.config/autofactory/config.toml` (Tier 1 machine config). `aaf telegram` fails closed if no chat IDs are allowlisted.

---

## 8. Keep `aaf` Up to Date (Fail-Closed Self-Upgrade)

`aaf upgrade` is **human-only** (refused inside agent runs when `AAF_RUN_ID` is set) and verifies SHA-256 checksums before replacing `aaf` and `aaf-hook` atomically:

```sh
# Check whether a newer version exists (read-only)
aaf upgrade --check

# Download and verify SHA-256 without overwriting binaries
aaf upgrade --dry-run

# Upgrade to the latest verified release
aaf upgrade

# Pin or roll back to a specific tag
aaf upgrade --version v0.1.0
```

---

## Next Steps

- Full configuration & safety model: [GUIDE.md](GUIDE.md)
- Security architecture & threat boundaries: [SECURITY.md](../SECURITY.md)
- Machine-readable automation: append `--json` to any command (except `aaf slot`).
