# Pātaka

A desktop AI chat client for macOS, Windows and Linux, grounded in Māori data sovereignty. Talk to Claude, OpenAI, DeepSeek, Qwen, Mistral or a model running on your own computer (Ollama, LM Studio). Before anything reaches a cloud model, [Tūmataiti](https://github.com/woodRock/tumataiti) redacts personal and culturally sensitive details **on your device**. Replies are restored locally, so you read a natural answer while the provider only ever sees placeholders.

Built with Rust and [Tauri 2](https://tauri.app). The window is plain HTML, CSS and JavaScript with no runtime npm dependencies.

![The review step: what Tūmataiti found, and exactly what will leave the device](docs/screenshots/review.png)

## What it does

- **Local redaction with Tūmataiti, rules and model.** The Tūmataiti rules find IRD numbers (mod-11 checked), NHI numbers, bank accounts, payment cards (Luhn checked), passports and driver licences, NZ phone numbers, emails, URLs, IP addresses, labelled dates of birth or death, and API keys or passwords. The **Tūmataiti name model** (DeBERTa-v3-small, fine-tuned on NZ and te reo Māori text) finds people's names, addresses and affiliation (iwi, hapū, marae) in context. It runs natively on your device through ONNX Runtime. **Kaitiaki lists** that you or your data steward maintain add names, places and tapu terms. Everything is merged by Tūmataiti's own review and merge steps. Each item is pseudonymised (`[PERSON_1]`), generalised (`[YEAR 1984]`), masked (`[NZ_GOV_ID]`) or blocked, per a policy you control.
- **Consistent placeholders across a kōrero.** The same person keeps the same placeholder across every turn. When a message goes to the cloud, the whole history is redacted again under the current policy, so a term added later also protects earlier turns.
- **Review before sending.** See exactly what the model will receive, with each placeholder highlighted. You can add something else to redact, or allow a term through, before you send.
- **Tapu and noa kōrero.** Mark a conversation *tapu* and Pātaka will only send it to a model on this device or on infrastructure you have marked *sovereign*. Pātaka works out "on this device" from the address; a remote server cannot claim it.
- **Where your data goes.** Every provider shows where it processes data. Every message records which provider and model handled it, and what left your device.
- **Encrypted at rest.** All kōrero, settings, API keys and placeholder mappings are encrypted with XChaCha20-Poly1305. The key is protected by your passphrase (Argon2id) or the OS keychain. There is no account, telemetry, analytics or sync.
- **Tamper-evident audit log.** A hash-chained record of what *kinds* of data went where and under which policy. It holds counts only, never text or placeholder values.
- **Yours to take or delete.** Export any kōrero as Markdown or JSON, set automatic deletion after N days, or delete everything (including the key) in one step.

See [docs/SOVEREIGNTY.md](docs/SOVEREIGNTY.md) for how this maps to the Te Mana Raraunga and CARE principles, and [SECURITY.md](SECURITY.md) for the security design and threat model.

## Install

**[Download Pātaka from the latest release](https://github.com/woodRock/Pataka/releases/latest)**, which has the installers for every platform and their checksums.

| Platform | Download | Also in this repo | Notes |
|---|---|---|---|
| macOS 13+ (Apple silicon and Intel) | [Pataka_0.2.0_universal.dmg](https://github.com/woodRock/Pataka/releases/latest/download/Pataka_0.2.0_universal.dmg) | (built by GitHub Actions) | Unsigned: right-click the app and choose *Open* the first time. |
| Windows 10/11 | [Pataka_0.2.0_x64-setup.exe](https://github.com/woodRock/Pataka/releases/latest/download/Pataka_0.2.0_x64-setup.exe) | [release/windows/](release/windows/Pataka_0.2.0_x64-setup.exe) | Installs for the current user. Unsigned, so SmartScreen may warn: choose *More info → Run anyway*. |
| Linux (Debian, Ubuntu) | [Pataka_0.2.0_amd64.deb](https://github.com/woodRock/Pataka/releases/latest/download/Pataka_0.2.0_amd64.deb) | [release/linux/](release/linux/Pataka_0.2.0_amd64.deb) | `sudo apt install ./Pataka_0.2.0_amd64.deb` |
| Linux (Fedora, openSUSE) | [Pataka-0.2.0-1.x86_64.rpm](https://github.com/woodRock/Pataka/releases/latest/download/Pataka-0.2.0-1.x86_64.rpm) | [release/linux/](release/linux/Pataka-0.2.0-1.x86_64.rpm) | `sudo dnf install ./Pataka-0.2.0-1.x86_64.rpm` |
| Linux (any) | [Pataka_0.2.0_amd64.AppImage](https://github.com/woodRock/Pataka/releases/latest/download/Pataka_0.2.0_amd64.AppImage) | [release/linux/](release/linux/Pataka_0.2.0_amd64.AppImage) | `chmod +x` it and run |

Checksums: [SHA256SUMS](https://github.com/woodRock/Pataka/releases/latest/download/SHA256SUMS) (also [release/SHA256SUMS](release/SHA256SUMS)). Check a download before installing:

```bash
sha256sum -c SHA256SUMS --ignore-missing     # Linux
shasum -a 256 -c SHA256SUMS --ignore-missing # macOS
```

### How the installers are built

- **Linux and Windows** are built locally with `scripts/build-linux.sh` and `scripts/build-windows.sh`, then committed to [`release/`](release/).
- **macOS** has to be built on a Mac, so it is built by the [macOS build](.github/workflows/macos.yml) workflow on a GitHub-hosted Mac. GitHub-hosted runners are free for public repositories, so no billing is needed. Run it from **Actions → macOS build → Run workflow**. With *publish* ticked, it creates the GitHub Release `v<version>` with the macOS `.dmg` and the Linux and Windows installers from `release/`, which is what the download links above point to. (GitHub only lists a workflow under Actions once it is on the default branch.)

## Getting started

1. Open Pātaka and choose how to protect your data: a **passphrase** (recommended) or the **OS keychain**.
2. Open **Settings → Providers**. For a local model, start [Ollama](https://ollama.com) and press **Fetch models** on the Ollama card. For a cloud model, paste an API key into its card. Keys are stored encrypted and never shown again.
3. Open **Settings → Tūmataiti** and press **Install** under *Name model*. This is a one-time download of about 170 MB (250 MB on Windows): the model from Hugging Face and ONNX Runtime from Microsoft's GitHub release. Every file is checked against a SHA-256 built into Pātaka.
4. Open **Settings → Kaitiaki lists** and add the names, marae, hapū, places and tapu terms that should always be protected, whatever the model finds.
5. Start a **Kōrero hou** (general) or a **Tapu** kōrero (on-device or sovereign models only).

## Build it yourself

You need [Rust](https://rustup.rs) and Node.js 20 or later (npm is only used to fetch the Tauri CLI).

```bash
npm ci
npx tauri dev          # run in development
scripts/check.sh       # fmt, clippy, tests, cargo audit/deny if installed, UI safety checks
```

Release builds:

| Target | Run on | Command |
|---|---|---|
| Linux .deb / .rpm / .AppImage | Linux | `scripts/build-linux.sh` |
| Windows NSIS installer | Linux (cross-compiled) | `scripts/build-windows.sh` |
| Windows NSIS + MSI | Windows | `powershell scripts/build-windows.ps1` |
| macOS universal .dmg | macOS, or GitHub Actions (*macOS build*) | `scripts/build-macos.sh` |

Each script copies its output into `release/<platform>/` and refreshes `release/SHA256SUMS`. Commit the result to publish it. Linux build dependencies are listed at the top of `scripts/build-linux.sh`.

Linux and Windows have no CI, so run `scripts/install-hooks.sh` once. It adds a git pre-push hook that runs `scripts/check.sh` before every push. (The macOS workflow runs the same checks before it builds.)

## Layout

```
src-tauri/src/
  redaction.rs     Tūmataiti rules + name model + kaitiaki lists, persistent placeholder vault, restore
  student.rs       Tūmataiti name model: tokenising, ONNX Runtime inference, span decoding (pinned hashes)
  model_install.rs one-time, hash-verified download of the model and ONNX Runtime
  sovereignty.rs   on-device / sovereign / offshore rules, tapu routing, URL checks
  crypto.rs        XChaCha20-Poly1305 and Argon2id
  store.rs         encrypted files, keychain or passphrase key, secure delete
  audit.rs         hash-chained audit log
  providers/       Anthropic and OpenAI-compatible streaming clients (the only network code)
  commands.rs      what the window may call
ui/                window: index.html, styles.css, app.js, markdown.js (safe, DOM-only renderer)
scripts/           build, checksum, check and hook scripts
release/           committed Linux and Windows installers and SHA256SUMS
.github/workflows/ macOS build on a GitHub-hosted Mac, publishes the GitHub Release
```

## Limits

Redaction reduces risk; it does not remove it.

- No detector catches everything. Tūmataiti's own evaluation finds that rules plus a student model still miss personal information in some held-out NZ documents (see its [results](https://github.com/woodRock/tumataiti/tree/main/ml/results)). Always read the outbound text in the review step.
- Without the name model installed (or on Intel Macs, which ONNX Runtime 1.31 has no build for), **names, addresses and whakapapa are only caught if they are on a kaitiaki list** or added in the review step.
- A model can sometimes infer who someone is from context alone.
- Once text reaches a provider, its own terms decide what happens to it.

## Licence

Apache 2.0. Tūmataiti is also Apache 2.0.
