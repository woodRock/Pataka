# Security

## Reporting a vulnerability

Please report security problems privately, through GitHub's **Security → Report a vulnerability** on this repository, rather than in a public issue. Include steps to reproduce if you can.

## Design

### Data at rest

| What | How it is protected |
|---|---|
| Kōrero (messages, placeholder mappings, per-kōrero terms) | One file per kōrero, encrypted with XChaCha20-Poly1305 under a random 256-bit data key. The associated data binds each file to its id, so files cannot be swapped or renamed into one another. |
| Settings, kaitiaki lists, **API keys** | `settings.ptk`, encrypted the same way. API keys are never sent to the window: it only learns whether a key is stored. |
| Data key | Either wrapped with a key derived from your passphrase by **Argon2id** (64 MiB, 3 passes, random salt), or stored in the **OS keychain** (macOS Keychain, Windows Credential Manager, Linux Secret Service). Switching to a passphrase removes it from the keychain. |
| Audit log | `audit.jsonl`, plain text so you can inspect it, but it holds only counts, categories, provider names and policy hashes: never message text or placeholder values. Entries are SHA-256 hash-chained, so editing or removing one is detected by **Verify chain**. |

Files are written atomically (temporary file, then rename) with owner-only permissions on macOS and Linux. Deleting a kōrero overwrites the file before removing it. On SSDs and copy-on-write file systems that is best effort; the encryption is what protects deleted data. The data key and API keys are held in memory types that are zeroed when dropped, and locking Pātaka drops them.

### Unlocking

- A wrong passphrase is reported without saying why decryption failed, and each failure doubles a delay before the next attempt (up to 30 seconds).
- Passphrases must be at least 12 characters with some variety.
- Pātaka locks after a period of inactivity (15 minutes by default, configurable) and on **Lock**.

### Network

- The window has no network access. Its Content-Security-Policy is `default-src 'self'`, with `connect-src` limited to Tauri's IPC, no inline scripts or styles, no frames, objects or workers, `base-uri 'none'` and `form-action 'none'`. Navigation away from the app's own pages is refused.
- All requests are made in Rust (`src-tauri/src/providers/`), and only after redaction and the sovereignty check. TLS is rustls with TLS 1.2 or later, trusting the operating system's certificate store (so enterprise TLS inspection works as it does for the rest of the system), with Mozilla's roots as a fallback. **Redirects are refused** so an API key cannot be carried to another host. Connect and read timeouts are bounded, and reply size is capped.
- Remote providers must use `https://`. Plain `http://` is accepted only for loopback addresses (`localhost`, `127.0.0.0/8`, `::1`). Credentials in URLs are rejected. Changing a provider's address deletes its stored key, so a key cannot be redirected by editing the URL.
- No telemetry, analytics, crash reporting, update checks or remote fonts.

### The Tūmataiti name model

- Installed only when you press **Install**. Downloads come from the pinned revision of `woodrock/tumataiti-deberta-v3-small-open` on Hugging Face, and from Microsoft's ONNX Runtime 1.31.0 release on GitHub. Redirects are followed only over HTTPS to those services' own download hosts.
- The SHA-256 of every file (model, tokeniser, config, ONNX Runtime archive and the extracted library) is compiled into Pātaka (`src-tauri/src/student.rs`). A download that does not match is discarded. The files are checked again each time the model loads, so a file changed on disk is refused.
- ONNX Runtime's telemetry is switched off explicitly.
- The model runs on this device. Its spans go through Tūmataiti's review step (a lone kinship word like *Koro* is flagged rather than redacted, and IDs are relabelled by their check digits), then merge with the rules.
- **Fail closed:** if the model is switched on and installed but cannot load or run, nothing is sent. The error says how to switch it off and send with the rules only.
- Detected spans are cached in memory by text hash to avoid re-running the model over the history. The cache is dropped on lock.
- On macOS, loading a library that was not signed with Pātaka needs the `com.apple.security.cs.disable-library-validation` entitlement (`src-tauri/Entitlements.plist`). The hash check before loading stands in for Apple's library validation.

### Agents (Claude Code in a project folder)

Agent kōrero run Claude Code as a full agent in a folder the person chooses (not their home folder or a drive root). Pātaka sits between Claude Code and the model:

- **Tūmataiti proxy.** Claude Code is started with `ANTHROPIC_BASE_URL` set to a proxy inside Pātaka, listening on 127.0.0.1 only, under a random path per session. Only the Messages API (and token counting) is forwarded; GETs pass through unchanged, and every other request is refused. The proxy refuses everything while Pātaka is locked.
- **Outbound.** Every request is redacted with the kōrero's placeholder vault: the system prompt, all messages, tool results (file contents, command output) and the arguments of earlier tool calls. The account name that appears in paths is treated as a name. Images and documents are withheld. "Block" actions become "mask" for content the agent read (a secret in a `.env` file is masked rather than ending the session), but tapu terms stop the request. The name model is not applied to the agent's own instructions or to code-like tokens such as paths, where it produces false matches; the rules, kaitiaki lists and identity terms still apply there.
- **Inbound.** Placeholders are restored in streamed text and in tool arguments, after each tool call is complete, so split placeholders are handled. Arguments are restored only for tools that act on this computer (Bash, Read, Write, Edit, MultiEdit, NotebookEdit, Glob, Grep, LS, TodoWrite, Task). WebFetch, MCP tools and any unknown tool keep their placeholders, so a restored value is never handed to a network tool. The one exception is Pātaka Mail (below), whose tools search the person's own mail server.
- **Approvals.** Claude Code's permission prompts are answered by Pātaka through its SDK control protocol: the person sees the exact command or diff, with real values, and allows or denies it. Permission modes are limited to ask, accept edits and plan; `bypassPermissions` is never used. "Always allow" lasts only for that session.
- **Background traffic.** `DISABLE_TELEMETRY`, `DISABLE_ERROR_REPORTING`, `DISABLE_AUTOUPDATER` and `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` are set for agent processes.
- **One agent per kōrero.** A second send while one is starting is refused. Locking Pātaka, wiping data or deleting the kōrero stops the agent and closes its proxy session.
- **Audit.** Each request through the proxy is logged with counts per category and how many images were withheld. Each message sent to an agent is logged too.

Limits: Claude Code stores its own session transcripts locally, unencrypted (`~/.claude/projects`). Hooks and MCP servers from the person's Claude Code settings still run, and their own network traffic bypasses Pātaka. An approved shell command runs with real values and could send them over the network.

### The `pataka` command (Claude Code in the terminal)

`pataka` is built from the same engine as the app (`crates/pataka-core`), so the proxy, redaction, vault, kaupapa, name model, guards and audit log are one implementation.

- `pataka claude` unlocks the store (OS keychain, or a passphrase typed at the terminal, which is never echoed or passed on), starts the proxy inside its own process on 127.0.0.1 behind a random path, and runs the real `claude` with `ANTHROPIC_BASE_URL` pointing at it. `DISABLE_TELEMETRY`, `DISABLE_ERROR_REPORTING` and `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` are set. The proxy stops when Claude Code exits. An existing `ANTHROPIC_BASE_URL` becomes the upstream, and must be https, or http on loopback.
- It shares the app's data folder, so the vault, lists and audit log are the same. The kaupapa is read afresh for every request, so a list change in either place applies at once. Placeholders for a folder persist in a "Terminal · folder" kōrero.
- **Hooks**, only installed with consent, merged into `~/.claude/settings.json` with a backup kept. The prompt hook asks the running proxy (a `pataka/check-prompt` endpoint on the same secret path) whether the prompt contains tapu terms, secrets or anything else the kaupapa blocks, so the hook never needs the passphrase. If the proxy cannot be reached, the prompt is refused. In a plain `claude` session it checks for secrets with the rules alone. The tool hook uses the same guards as the app's agents and answers "ask"; it never answers "allow".
- `pataka setup` ends with a self-test: a request containing an email address and an IRD number goes through the real proxy to a pretend model on 127.0.0.1. Setup fails unless only placeholders arrive and the reply comes back restored.
- **Install scripts** download from this repository's `release/cli/` folder and check the SHA-256 listed in `release/SHA256SUMS` before installing for the current user only. Both files come from the same place, so this catches a damaged download, not a compromised repository. For that, compare against a checksum you got elsewhere, or build from source (`cargo install --git … pataka-cli`).
- Claude Code's own transcripts in `~/.claude/projects` hold real values and are not encrypted by Pātaka (see `docs/CLAUDE-CODE.md`).

### Pātaka Mail (Claude Code and an IMAP mailbox)

`pataka claude` can give Claude Code tools to read and organise the person's mailbox (`docs/CLAUDE-CODE.md`, "Mail").

- **Account.** `pataka mail connect` reads the IMAP app password without echo, tests the login, and stores the account in `mail.ptk`, encrypted with the data key like the settings. Connecting and disconnecting are audited by server name only.
- **Where it runs.** The MCP server runs inside the `pataka` process, on the agent proxy at `{base}/pataka/mcp`: 127.0.0.1 only, behind the session's secret path. It uses the store key that is already unlocked, so there is no second unlock, and it exposes no restore endpoint. Claude Code is registered with an inline `--mcp-config` whose address is the environment variable `PATAKA_MAIL_URL`, expanded by Claude Code, so the secret path never appears in the process list. Requests carrying an `Origin` header are refused, and sessions that did not register mail get a 404.
- **Restoring.** The proxy restores placeholders in tool arguments for `mcp__pataka-mail__*` only, and only in sessions that registered the server. Every other MCP tool still keeps its placeholders. Real values therefore reach only the mail server the person connected.
- **Redacting.** Results go back to Claude Code and then, as tool results, through the proxy, which redacts them like file contents before Anthropic sees them. There is no second redaction step. Results are plain text, bodies are cut at about 20 KB, and attachments are listed by name and size, never read.
- **Network.** IMAP over TLS 1.2 or later (rustls, the operating system's roots with Mozilla's as a fallback), only to the configured server. Plain TCP is accepted only for a loopback address, which the tests use.
- **Prompt injection.** Email is untrusted input. `read` labels it as such for the model. The guards (`crates/pataka-core/src/guards.rs`), in the app and in the PreToolUse hook, ask for every change in auto mode, for every change touching more than 25 messages, and for every `trash`. `trash` is never "always allowed". With permission prompts switched off, changes that need a yes are refused. `pataka claude` installs this hook for the session through `--settings`, scoped to `mcp__pataka-mail__.*`, so it does not depend on `pataka hooks install`.
- **Audit and undo.** Each change is logged with its action, message ids and, for labels and folders, a keyed hash of the name plus a reference into `mail.ptk`. Subjects, addresses and message text are never logged. `pataka mail undo <entry>` checks the audit chain, reverses the change and logs the undo.
- **Other mail paths.** With a mailbox connected, `pataka claude` sets `ENABLE_CLAUDEAI_MCP_SERVERS=false`, so claude.ai connectors (which would read mail on Anthropic's side, unredacted) are not available in that session. The Tūmataiti mod restores `mcp__pataka-mail__*` arguments only when `PATAKA_MAIL_URL` points at a loopback `/pataka/mcp` address.
- **Drafts and reading.** `draft` appends a plain-text message to the Drafts folder (header values are stripped of line breaks; recipients must be addresses). It is a change like any other: guarded, audited as "1 draft" with its folder reference and UID only, and undone by moving it to the Trash. Every fetch uses `BODY.PEEK`, so reading never marks mail as read.
- **Not included:** sending, permanent deletion, OAuth or the Gmail API, and any use from the desktop app.

Limits: an MCP server with the same name (`pataka-mail`) configured in Claude Code's own settings could conflict with Pātaka's. Pātaka only restores values for sessions it registered, but people should not add such a server.

### Subscriptions through Claude Code and Codex

Pātaka can use a Claude or ChatGPT subscription by running the vendor's own signed-in command-line app (`claude` or `codex`), the way T3 Code does. Pātaka never handles that login; the app keeps its own credentials. Each message starts a fresh run:

- Only Tūmataiti-redacted text is sent, and the whole kōrero is redacted again first, exactly as for the APIs. These connections always count as offshore, so tapu kōrero cannot use them.
- The prompt goes in on standard input, not the command line, where other users of the computer could see it. Each run happens in a new, empty temporary folder that is deleted afterwards.
- **Claude Code** runs with `--tools ""` (no tools at all), `--strict-mcp-config` (no MCP servers), `--setting-sources ""` (no user or project settings, hooks or CLAUDE.md files), `--disable-slash-commands` and `--no-session-persistence`, with Pātaka's system prompt replacing Claude Code's. If its start-up event lists any tool, or it starts a tool call, Pātaka stops it.
- **Codex** has no way to switch its command tool off. It runs with `--sandbox read-only`, `--ephemeral`, `--ignore-rules`, no inherited environment for commands, and browser, computer use and apps disabled. Pātaka watches its event stream and kills it the moment it starts a command, file change, web search or tool call. That is best effort: in the moment before it is stopped, a command could read a file. The connect dialog says so and asks for confirmation.
- Whether a vendor allows its subscription to be used through another app is set by its terms. Check them.

### The window (Tauri)

- The Tauri capability for the window grants only `core:event:allow-listen` and `allow-unlisten`. No file system, shell, HTTP, opener or clipboard plugins are exposed to JavaScript. The save dialog for exports is opened from Rust.
- `freezePrototype` is on. Model output is rendered by `ui/markdown.js`, which builds DOM nodes with `textContent` only and never parses HTML. Links are shown as text, not made clickable, and images are never loaded. That closes the exfiltration route Tūmataiti's threat model names, where a reply puts a placeholder into a URL so that restoring it sends the real value somewhere.
- `scripts/check.sh` fails if `innerHTML`, `eval` or similar appear in `ui/`, or if inline scripts or styles appear in `index.html`.
- Placeholders are restored for display only, and only those this kōrero issued. A reply cannot introduce a mapping of its own.

### Supply chain

- `Cargo.lock` and `package-lock.json` are committed. Tūmataiti is pinned to a specific git commit.
- `src-tauri/deny.toml` restricts licences, git sources and yanked crates for `cargo deny`. `scripts/check.sh` runs `cargo audit` and `cargo deny` when they are installed.
- The UI has no npm runtime dependencies. npm is used only to fetch the Tauri CLI at build time.
- Installers in `release/` have SHA-256 checksums in `release/SHA256SUMS`. They are **not code-signed** (no Apple Developer ID or Windows certificate), so verify the checksum before installing.

## Threat model

| Threat | Mitigation |
|---|---|
| A cloud provider logs or retains prompts | Only Tūmataiti-redacted text leaves the device. Tapu kōrero only go to on-device or sovereign providers. |
| An agent's file contents or command output reach the model | All agent traffic goes through the local Tūmataiti proxy and is redacted; images are withheld |
| An agent sends a restored value to the network | Only local tools get restored arguments; web and MCP tools keep placeholders; commands need approval |
| An email carries instructions aimed at the agent (prompt injection) | Mail changes ask in auto mode, above 25 messages and for every `trash`; no sending or permanent deletion; every change is audited and can be undone |
| Restored names reach the mail server | Only `pataka-mail` tools get restored arguments, only in sessions that registered it, only to the configured IMAP server over TLS |
| Another local program uses the agent proxy | Bound to 127.0.0.1, random per-session path, closed when the session ends or Pātaka locks |
| A subscription app (coding agent) acts on the computer | Claude Code runs with no tools; Codex runs read-only and is stopped as soon as it starts an action |
| Missed personal data (false negatives) | Checksum-validated rules, the Tūmataiti name model, kaitiaki lists, a review step that shows the full outbound text, and fail-closed blocking for secrets and tapu terms |
| Tampered model or runtime files | Pinned SHA-256 for every file, checked at download and at every load |
| An earlier on-device conversation is later sent to the cloud | The whole history is redacted again whenever the destination is off the device |
| Someone with access to the disk or a backup | Everything is encrypted. With a passphrase, the data cannot be read without it. |
| Malware running as the same user | Out of scope once Pātaka is unlocked. With a passphrase, data at rest is still protected while locked. |
| A reply tries to exfiltrate restored values through links or images | Nothing in a reply is fetched or made clickable. Navigation is locked. The CSP blocks other connections. |
| Placeholder injection in replies | Only placeholders issued in this kōrero are restored |
| An API key leaks via a redirect or misconfigured URL | Redirects are refused, https is required, and keys are dropped when a provider's address changes |
| The audit log is edited to hide what was sent | The hash chain detects edits and deletions |
| The model infers identity from context | Not prevented. The review step and the tapu classification are the controls. |

## Known limits

- No detector is perfect. Without the name model (not installed, switched off, or on Intel Macs), names and addresses are only detected through kaitiaki lists.
- The system prompt is not redacted; keep personal details out of it.
- Installers are unsigned.
- If you forget your passphrase, your data cannot be recovered. That is deliberate.
