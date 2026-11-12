# Tūmataiti mod for Claude Code

A Claude Code mod that redacts personal and culturally sensitive details before anything is stored or sent to the model, including in the Claude desktop app's **Code** tab, which the Pātaka proxy cannot reach.

- **Typed prompts** are redacted before they are shown, stored or sent. A tapu term stops the prompt.
- **Everything else entering the conversation** (file contents, command output, attachments, injected context) is redacted before it is stored. Images and documents are withheld, because they cannot be checked.
- **Local tools** (Bash, Read, Write, Edit, Glob, Grep…) get the real values back in their arguments just before they run, so Claude still works on your real files. Network and MCP tools keep the placeholders, except Pātaka Mail's own tools in a `pataka claude` session that registered them (see `docs/CLAUDE-CODE.md`, "Mail").

It uses the Pātaka service for the Tūmataiti rules, the name model, your kaitiaki lists and the placeholder vault, so set that up first:

```bash
pataka service install
pataka unlock          # after each login, if you use a passphrase
```

It fails closed where Claude Code allows: a prompt Pātaka cannot check is not sent, a tool result it cannot check is withheld, and a tool whose arguments it cannot restore does not run. If the mod itself is not loaded, nothing is redacted. It does not cover the desktop app's **Chat** tab, which does not run Claude Code.
