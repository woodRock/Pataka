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
- All requests are made in Rust (`src-tauri/src/providers/`), and only after redaction and the sovereignty check. TLS is rustls with TLS 1.2 or later. **Redirects are refused** so an API key cannot be carried to another host. Connect and read timeouts are bounded, and reply size is capped.
- Remote providers must use `https://`. Plain `http://` is accepted only for loopback addresses (`localhost`, `127.0.0.0/8`, `::1`). Credentials in URLs are rejected. Changing a provider's address deletes its stored key, so a key cannot be redirected by editing the URL.
- No telemetry, analytics, crash reporting, update checks or remote fonts.

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
| Missed personal data (false negatives) | Checksum-validated rules, kaitiaki lists, a review step that shows the full outbound text, and fail-closed blocking for secrets and tapu terms |
| An earlier on-device conversation is later sent to the cloud | The whole history is redacted again whenever the destination is off the device |
| Someone with access to the disk or a backup | Everything is encrypted. With a passphrase, the data cannot be read without it. |
| Malware running as the same user | Out of scope once Pātaka is unlocked. With a passphrase, data at rest is still protected while locked. |
| A reply tries to exfiltrate restored values through links or images | Nothing in a reply is fetched or made clickable. Navigation is locked. The CSP blocks other connections. |
| Placeholder injection in replies | Only placeholders issued in this kōrero are restored |
| An API key leaks via a redirect or misconfigured URL | Redirects are refused, https is required, and keys are dropped when a provider's address changes |
| The audit log is edited to hide what was sent | The hash chain detects edits and deletions |
| The model infers identity from context | Not prevented. The review step and the tapu classification are the controls. |

## Known limits

- Names and addresses are only detected through kaitiaki lists. The Tūmataiti name model is not bundled yet.
- The system prompt is not redacted; keep personal details out of it.
- Installers are unsigned.
- If you forget your passphrase, your data cannot be recovered. That is deliberate.
