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

## Every terminal session, without `pataka claude`

`pataka claude` only covers the Claude Code you start with it. To cover a plain `claude` too, in any folder, run Pātaka as a service:

```bash
pataka service install     # asks first; unlock once if you use a passphrase
pataka unlock              # after each login, if you use a passphrase
```

`pataka service install` does four things:

- starts `pataka serve` at login (a launchd agent on macOS, a systemd user unit on Linux), listening on `127.0.0.1:47613` only, behind a secret path;
- sets `ANTHROPIC_BASE_URL` and `PATAKA_HOOK_URL` in the `env` of `~/.claude/settings.json`, so Claude Code sessions started from a terminal send their requests through the service;
- switches off Claude Code's telemetry and error reports there (`DISABLE_TELEMETRY`, `DISABLE_ERROR_REPORTING`, `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`), because that traffic would not pass through Pātaka;
- adds the hooks below, so typed prompts are checked against your whole kaupapa, including tapu terms.

**It does not cover the Claude desktop app.** The app starts Claude Code with its own `ANTHROPIC_BASE_URL`, which takes precedence over `settings.json`, so its Code and Chat tabs still send unredacted text to Anthropic. For redacted agent work, use `pataka claude` in a terminal or an agent kōrero in the Pātaka app.

With the service installed, plain `claude` is enough: `pataka claude` is not needed.

It **fails closed**. While the service is locked, Claude Code shows *"Pātaka is locked, so nothing was sent"*. While it is stopped, Claude Code cannot connect and keeps retrying. Either way nothing leaves unredacted. If Claude Code seems stuck, check with `pataka service status`.

```bash
pataka service status      # running? unlocked? is Claude Code pointed at it?
pataka lock                # refuse every request until `pataka unlock`
pataka service stop        # or start
pataka service uninstall   # puts Claude Code's settings back as they were
```

The placeholders for these sessions live in one kōrero, *Claude Code · all sessions*, so a name keeps the same placeholder across sessions and projects. If you already pointed Claude Code at a gateway with `ANTHROPIC_BASE_URL`, the service sends redacted requests on to it. Windows does not start the service at login yet: run `pataka serve` yourself, for example from Task Scheduler.

## Hooks

`pataka hooks install` (or `pataka setup`, or the app's button) adds two hooks to `~/.claude/settings.json`. It asks first, keeps your other settings and hooks, and saves a backup of the file:

- **Before a prompt is sent:** refuse it if it contains a term on your tapu list, or what looks like a password or API key. Under `pataka claude` or the service, the running proxy checks the prompt against your kaupapa, so the hook never needs your passphrase. In a plain `claude` session, only the check for secrets applies.
- **Before a tool runs** (optional): Claude Code asks you first before commands that can send data off the computer (`curl`, `wget`, `scp`, `ssh`, `nc`, `git push`, `gh`, publishing commands and the like), anything that reaches outside the project folder, web fetches and MCP tools, even if you have allowed that tool before. In auto mode (and with permission prompts switched off) the tool guard steps aside and Claude Code's own auto-mode checks decide, so it does not prompt on every step. Pātaka Mail's changes are the exception: they are checked in every mode (see [Mail](#mail)). Redaction still applies: it happens in the proxy, not the hook.

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

## Mail

Pātaka Mail lets Claude Code, run through `pataka claude`, read, search and organise your mailbox over IMAP: Gmail, or any other IMAP server. Anthropic only ever sees redacted text. Real names are filled back in on your computer just before Pātaka talks to your mail server.

### Connect

```bash
pataka mail connect                          # Gmail (imap.gmail.com:993)
pataka mail connect --host mail.example.nz   # another IMAP server, port 993 over TLS
pataka mail status
pataka mail disconnect
```

For Gmail, switch IMAP on in Gmail's settings and create an **app password** (Google Account → Security → 2-Step Verification → App passwords). `pataka mail connect` asks for your address and the app password. The password is not shown as you type it. Pātaka tests the login, then stores the account encrypted in its data folder (`mail.ptk`, under the same key as the rest of the store). The audit log records that a mailbox was connected or disconnected, and to which server. It never records the password.

### Use

Run `pataka claude` as usual. While a mailbox is connected, Claude Code gets the `pataka-mail` tools:

| Tool | What it does | Asks you first |
|---|---|---|
| `search` | Gmail search syntax (`from:`, `label:`, `is:unread`, `after:2026/01/31` …). Lists id, date, sender, subject, labels and a snippet, up to 50 at a time | no |
| `read` | One message as plain text, with attachment names and sizes | no |
| `thread` | A whole Gmail thread, oldest first: every message with date, sender, recipients, labels and UNREAD or read | no |
| `drafts` | Drafts, newest first (a message still in Drafts has not been sent) | no |
| `list_labels` | Labels or folders, with counts | no |
| `label` | Add or remove labels | as your Claude Code settings say |
| `move` | Move to a label or folder | as your Claude Code settings say |
| `archive` | Take messages out of the Inbox | as your Claude Code settings say |
| `mark` | Read, unread, starred or unstarred | as your Claude Code settings say |
| `create_label` | Create a label (`/` nests it) | as your Claude Code settings say |
| `draft` | Save a plain-text draft in Drafts, optionally as a reply in a thread. It is never sent: you send it from Gmail | as your Claude Code settings say |
| `trash` | Move to the Trash | always |

Search results carry a message id (`mail_…`) and a thread id (`thread_…`), and are grouped by thread. Labels are addressed by name, and nested labels use `/` (`Side Quests/TODO`). In a search they are written Gmail's way: `label:side-quests-todo`. Reading, whether with `read`, `thread` or `search`, never marks a message as read.

For example: *"Label everything from Aroha about the hui as Whānau, and archive the newsletters from last month."*

Pātaka always asks before a change:

- that touches **more than 25 messages**. It tells you how many, for example *"this archives 214 emails"*;
- that moves messages to the **Trash**. It asks every time, and saying yes once does not allow it again;
- of any kind in **auto mode**. Email can contain instructions aimed at Claude, and auto mode's own checks read that same email. So every change needs your explicit yes;
- of any kind with permission prompts switched off (`bypassPermissions` or `dontAsk`). There is nobody to ask, so the change is refused. Switch to the default mode or auto mode to approve it.

`pataka claude` adds this check to the session itself, so it applies whether or not you ran `pataka hooks install`.

While a mailbox is connected, `pataka claude` also switches off claude.ai connectors (such as the Gmail connector) for its sessions, by setting `ENABLE_CLAUDEAI_MCP_SERVERS=false`. Those connectors run on Anthropic's side and would read your mail there without redaction. So Claude has only one way to reach your mail: `pataka-mail`.

If you also use the Tūmataiti mod, it redacts tool results inside Claude Code before Claude Code stores them. In a `pataka claude` session with mail (where `PATAKA_MAIL_URL` is set), the mod puts real values back into `pataka-mail` arguments as well, so message ids and names survive the round trip. Update the mod to 0.1.1 or later.

### Inbox triage

The Tūmataiti mod (0.1.2 or later) includes an inbox-triage skill that you start by hand. It labels new mail, archives what needs no action from you, checks each open TODO for completion (by reading the whole thread, and checking Sent and Drafts), clears TODO labels from threads you have archived, and replies with a numbered **bounty list**:

```bash
pataka claude
> /tumataiti:inbox-triage
```

It reads your own setup from `~/.claude/inbox-triage.md`: your label names, what belongs under each, and "hold rules" (people whose unread mail must stay in the inbox). That file is yours. It is not part of Pātaka and is never committed. Claude Code asks before reading it, because it is outside the project folder. To stop being asked, allow `Read(~/.claude/inbox-triage.md)` in your Claude Code permissions.

The skill only labels and archives. It never sends, deletes, trashes or marks messages as read. Archiving more than 25 threads at once asks you first, as any bulk change does. The report is shown in your terminal with real names, but Anthropic only saw placeholders. It lists events worth adding to your calendar instead of adding them, because a calendar connector would only receive placeholders.

### Undo

Every change is written to the audit log. Its entry number appears in the tool's result, and in `pataka audit show` (as `mail-label`, `mail-archive` and so on). To reverse one change:

```bash
pataka mail undo 42
```

This puts back the labels, flags or folder the messages had, and writes its own audit entry. A label created by `create_label` is only removed if it is still empty.

### What goes where

- **Anthropic** sees search results and messages only after the proxy has redacted them, as for file contents. Message bodies are cut at about 20 KB. Attachments are listed by name and size only: their content is never read or sent.
- **Your mail server** receives real values, such as the name you search for or a label name, over TLS. Pātaka connects only to the server you connected, and to no other host.
- **The audit log** holds the action, message ids and, for each label or folder, a keyed hash plus a reference into the encrypted `mail.ptk`. It never holds subjects, addresses or message text.
- **Claude Code's transcripts** (`~/.claude/projects`) hold the messages it read, with real values, just as they do for files.

### Limits

- Pātaka Mail cannot send email or delete it permanently (`draft` only saves to Drafts), and it does not use OAuth or the Gmail API. The Pātaka app does not offer it yet, and neither does a plain `claude` under `pataka service`.
- On Gmail, search covers All Mail, which leaves out Spam and Trash.
- On other servers, labels are folders. `label` is not available there, `move` needs the server's MOVE support, and `archive` needs an Archive folder.
- Pātaka restores placeholders only for tools named `mcp__pataka-mail__*`, and only in sessions it registered the server for. Do not add another MCP server called `pataka-mail` to your Claude Code settings.

## Check what was sent

To see exactly what leaves your computer, start `pataka claude` with a request log:

```bash
PATAKA_REQUEST_LOG=~/pataka-sent.jsonl pataka claude
```

Each request to the model is appended to that file, one JSON line per request, *after* redaction: it is what Anthropic receives. Search it for a name you expect to be hidden, and you should find only placeholders. The file is created readable by you only, and it holds no real values. Delete it when you are done.

## What stays on your computer

Be clear about where things live:

- **Claude Code keeps its own transcripts** of each session in `~/.claude/projects` (Windows: `%USERPROFILE%\.claude\projects`), unencrypted, **with real values**. That is because Pātaka fills the real values back in before Claude Code sees them. It is the same as using Claude Code directly, and these files stay on your computer, but treat that folder as sensitive: back it up and delete it as you would the project. Pātaka does not encrypt or remove them.
- **Pātaka's own store** (the placeholder vault, kaitiaki lists and settings) is encrypted in Pātaka's data folder. The audit log there records counts and destinations, never text.
- **Anthropic receives only redacted text**: your prompts, file contents, command output and Claude Code's own instructions, with placeholders in place of personal and culturally sensitive details. Images and PDFs are withheld, because they cannot be checked. Secrets that Claude Code reads (an API key in a `.env` file, for example) are masked.

## Limits

- No detector catches everything. With the name model installed, most names, addresses and affiliations are caught in context. Without it, only your kaitiaki lists catch them. Rules always cover NZ identifiers, contact details and secrets.
- Hooks and MCP servers in your Claude Code settings still run, and their own network traffic does not pass through Pātaka. Pātaka never gives them restored values: arguments to MCP tools and web fetches keep their placeholders. The one exception is Pātaka Mail's own tools (see [Mail](#mail)), which search your own mail server.
- A command you approve runs with real values. A command that uses the network could send them somewhere, which is why the tool hook asks about those commands.
- If a tapu term reaches Claude Code's context (it read a file containing one), every later request in that session is refused. Start a new session.
