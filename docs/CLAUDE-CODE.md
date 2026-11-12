# Use Pātaka with Claude Code

`pataka claude` runs the real Claude Code with Pātaka's Tūmataiti proxy in the middle. Claude Code looks and behaves exactly as usual, and all its own arguments work. The difference is what reaches Anthropic: only redacted text. Names, contact details, NZ identifiers, iwi, hapū and marae, and anything on your kaitiaki lists are replaced with placeholders such as `[PERSON_1]` before they leave your computer. They are filled back in on your computer before Claude Code shows a reply or runs a tool.

## Install

With the Pātaka app: **Settings → Claude Code → Use Tūmataiti with Claude Code in the terminal**. This installs the `pataka` command and, if you agree, adds the hooks below.

Without the app:

```bash
# macOS and Linux
curl -fsSL https://raw.githubusercontent.com/woodRock/Pataka/main/install.sh | sh
```

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/woodRock/Pataka/main/install.ps1 | iex
```

The installer downloads the prebuilt `pataka` for your computer from this repository's `release/cli/` folder and checks it against `release/SHA256SUMS` before installing it into `~/.local/bin` (Windows: `%LOCALAPPDATA%\Pataka\bin`). With Rust installed you can build it yourself instead: `cargo install --git https://github.com/woodRock/Pataka pataka-cli`.

Then run the first-time setup:

```bash
pataka setup
```

It asks how to protect your data (a passphrase, or the OS keychain), offers to install the Tūmataiti name model, offers the Claude Code hooks, and finishes by checking that a test request really is redacted on its way out and restored on its way back. If you use the Pātaka app too, both share one data folder: the same vault, kaitiaki lists, name model and audit log.

## Use

In a project folder:

```bash
pataka claude                 # instead of: claude
pataka claude --model opus    # Claude Code's own arguments pass straight through
pataka claude -p "summarise notes.txt"
```

If you use a passphrase, `pataka` asks for it when it starts. Claude Code's telemetry, error reporting and other background traffic are switched off for the session. If you already set `ANTHROPIC_BASE_URL` (for example to a company gateway), Pātaka sends redacted requests on to it.

To point some other tool, or a Claude Code you start yourself, at the proxy:

```bash
pataka proxy        # prints an ANTHROPIC_BASE_URL to use; Ctrl-C stops it
```

## Every session, including the Claude desktop app

`pataka claude` only covers the Claude Code you start with it. The Claude desktop app (its **Code** tab) starts Claude Code itself, so for that, and for a plain `claude`, run Pātaka as a service:

```bash
pataka service install     # asks first; unlock once if you use a passphrase
pataka unlock              # after each login, if you use a passphrase
```

`pataka service install` does four things:

- starts `pataka serve` at login (a launchd agent on macOS, a systemd user unit on Linux), listening on `127.0.0.1:47613` only, behind a secret path;
- sets `ANTHROPIC_BASE_URL` and `PATAKA_HOOK_URL` in the `env` of `~/.claude/settings.json`, so every Claude Code session on this computer, in the terminal or the desktop app, sends its requests through the service;
- switches off Claude Code's telemetry and error reports there (`DISABLE_TELEMETRY`, `DISABLE_ERROR_REPORTING`, `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`), because that traffic would not pass through Pātaka;
- adds the hooks below, so typed prompts are checked against your whole kaupapa, including tapu terms.

Restart the desktop app, or open a new session, to pick it up. With the service installed, plain `claude` is enough: `pataka claude` is not needed.

It **fails closed**. While the service is locked, Claude Code shows *"Pātaka is locked, so nothing was sent"*. While it is stopped, Claude Code cannot connect and keeps retrying. Either way nothing leaves unredacted. If Claude Code seems stuck, check with `pataka service status`.

```bash
pataka service status      # running? unlocked? is Claude Code pointed at it?
pataka lock                # refuse every request until `pataka unlock`
pataka service stop        # or start
pataka service uninstall   # puts Claude Code's settings back as they were
```

The placeholders for these sessions live in one kōrero, *Claude Code · all sessions*, so a name keeps the same placeholder across sessions and projects. If you already pointed Claude Code at a gateway with `ANTHROPIC_BASE_URL`, the service sends redacted requests on to it. Windows does not start the service at login yet: run `pataka serve` yourself, for example from Task Scheduler.

The service does not cover the desktop app's **Chat** tab, which does not use Claude Code.

## Hooks

`pataka hooks install` (or `pataka setup`, or the app's button) adds two hooks to `~/.claude/settings.json`. It asks first, keeps your other settings and hooks, and saves a backup of the file:

- **Before a prompt is sent:** refuse it if it contains a term on your tapu list, or what looks like a password or API key. Under `pataka claude` or the service, the running proxy checks the prompt against your kaupapa, so the hook never needs your passphrase. In a plain `claude` session, only the check for secrets applies.
- **Before a tool runs** (optional): Claude Code asks you first before commands that can send data off the computer (`curl`, `wget`, `scp`, `ssh`, `nc`, `git push`, `gh`, publishing commands and the like), anything that reaches outside the project folder, web fetches and MCP tools, even if you have allowed that tool before.

`pataka hooks uninstall` removes them.

## Configure

```bash
pataka lists add-person "Hēmi Te Rangi"
pataka lists add-affiliation "Ngāti Kahungunu"
pataka lists add-place "Kahuranaki Marae"
pataka lists add-tapu "karakia tapu"        # a request containing it is never sent
pataka lists add-allow "Āpirana Ngata"      # may be sent unchanged (e.g. a tupuna in research)
pataka lists show
pataka lists remove "Hēmi Te Rangi"

pataka model install       # the Tūmataiti name model, about 170 MB, SHA-256 pinned
pataka model status

pataka audit verify        # check the hash chain
pataka audit show          # recent entries: counts and destinations, never text
pataka status              # what is set up
```

Changes apply straight away, in the terminal and in the app.

## What stays on your computer

Be clear about where things live:

- **Claude Code keeps its own transcripts** of each session in `~/.claude/projects` (Windows: `%USERPROFILE%\.claude\projects`), unencrypted, **with real values**. That is because Pātaka fills the real values back in before Claude Code sees them. It is the same as using Claude Code directly, and these files stay on your computer, but treat that folder as sensitive: back it up and delete it as you would the project. Pātaka does not encrypt or remove them.
- **Pātaka's own store** (the placeholder vault, kaitiaki lists and settings) is encrypted in Pātaka's data folder. The audit log there records counts and destinations, never text.
- **Anthropic receives only redacted text**: your prompts, file contents, command output and Claude Code's own instructions, with placeholders in place of personal and culturally sensitive details. Images and PDFs are withheld, because they cannot be checked. Secrets that Claude Code reads (an API key in a `.env` file, for example) are masked.

## Limits

- No detector catches everything. With the name model installed, most names, addresses and affiliations are caught in context. Without it, only your kaitiaki lists catch them. Rules always cover NZ identifiers, contact details and secrets.
- Hooks and MCP servers in your Claude Code settings still run, and their own network traffic does not pass through Pātaka. Pātaka never gives them restored values: arguments to MCP tools and web fetches keep their placeholders.
- A command you approve runs with real values. A command that uses the network could send them somewhere, which is why the tool hook asks about those commands.
- If a tapu term reaches Claude Code's context (it read a file containing one), every later request in that session is refused. Start a new session.
