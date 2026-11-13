# Tūmataiti mod for Claude Code

A Claude Code mod that redacts personal and culturally sensitive details before anything is stored or sent to the model, including in the Claude desktop app's **Code** tab, which the Pātaka proxy cannot reach.

- **Typed prompts** are redacted before they are shown, stored or sent. A tapu term stops the prompt.
- **Everything else entering the conversation** (file contents, command output, attachments, injected context) is redacted before it is stored. A PDF becomes its text, read on this computer by the Pātaka service and redacted with the rest; images and scanned PDFs are withheld, because they cannot be checked yet.
- **Local tools** (Bash, Read, Write, Edit, Glob, Grep…) get the real values back in their arguments just before they run, so Claude still works on your real files. Network and MCP tools keep the placeholders. Pātaka Mail's own tools do too: its server puts the real values back itself (see `docs/CLAUDE-CODE.md`, "Mail").
- **Claude's replies** are shown to you with the real values back in, while the stored reply keeps its placeholders, so the model never reads them. Only the transcript text changes: the app's own summary boxes and tool rows still show placeholders.

It uses the Pātaka service for the Tūmataiti rules, the name model, your kaitiaki lists and the placeholder vault, so set that up first:

```bash
pataka service install
pataka unlock          # after each login, if you use a passphrase
```

It fails closed where Claude Code allows: a prompt Pātaka cannot check is not sent, a tool result it cannot check is withheld, and a tool whose arguments it cannot restore does not run. If the mod itself is not loaded, nothing is redacted. It does not cover the desktop app's **Chat** tab, which does not run Claude Code.
