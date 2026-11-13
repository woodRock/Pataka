// Tūmataiti for Claude Code, as a mod.
//
// Everything that enters the conversation (typed prompts, file contents,
// command output, attachments) is sent to the Pātaka service on this computer
// and comes back with personal details replaced by placeholders such as
// [PERSON_1]. Only those rows are stored, so only they are ever sent to the
// model. Before a local tool runs (Bash, Read, Edit…), the placeholders in its
// arguments are put back, so Claude still works on the real files.
//
// It fails closed where the API allows it: a prompt Pātaka cannot check is not
// sent, a tool result it cannot check is withheld, and a tool whose arguments
// it cannot restore does not run. Needs `pataka service install` and an
// unlocked service (`pataka unlock`).

import type { EngineInterface, Register } from 'claude-code'

type Json = null | boolean | number | string | Json[] | { [k: string]: Json }
type Block = { type: string; [field: string]: unknown }
type Dollar = EngineInterface

const LOCKED_HINT = 'Run `pataka unlock` in a terminal (or `pataka service status` to check).'
const WITHHELD = '[withheld by Pātaka: it could not be checked for personal information]'
const MEDIA = '[image or document withheld by Pātaka: it cannot be checked for personal information]'

/** Built-in tools that act only on this computer: their arguments get real values back. */
const LOCAL_TOOLS = new Set([
  'Bash', 'BashOutput', 'KillShell', 'Read', 'Write', 'Edit', 'MultiEdit',
  'NotebookEdit', 'NotebookRead', 'Glob', 'Grep', 'LS',
])
/**
 * Pātaka Mail's tools keep their placeholders: its server puts the real
 * values back itself when the call arrives. Rewriting them here would change
 * a call after Claude Code's auto mode reviewed it, and auto mode refuses to
 * judge a call a hook changed. Every other MCP tool keeps its placeholders
 * too.
 */
function restoresInput(tool: string): boolean {
  return LOCAL_TOOLS.has(tool)
}

/** Keys of tool.call's input that are not the tool's own arguments. */
const RESERVED = new Set(['tool', 'tool_use_id', 'agentId', 'requestMeta', 'consent'])
/** Rows the model never reads, or that it wrote itself from placeholders. */
const SKIP_DOORS = new Set(['response', 'compaction', 'notice'])
/** A placeholder; forms of one person's name add a letter to the number. */
// `[literal …]` is text that only looked like a placeholder; restoring it
// gives back what was written.
const PLACEHOLDER = /\[(?:literal )*[A-Z][A-Z_]*_\d+[A-Z]?\]/

/** `pataka run` puts placeholders back itself, as the command runs. */
const PATAKA_RUN = /(?:^|[\s;&|(])pataka\s+run(?:\s|$)/

/** Whether a Bash call goes through `pataka run`: its input stays as written. */
function runsThroughPataka(args: Record<string, unknown>): boolean {
  return typeof args.command === 'string' && PATAKA_RUN.test(args.command)
}

// Example placeholders for the note, built here so this file holds none.
const eg = (label: string, n: string) => `[${label}_${n}]`

/** What the model is told about placeholders, as a system prompt section. */
export const NOTE = [
  `Privacy note from Pātaka: text in square brackets such as ${eg('PERSON', '1')} or ${eg('EMAIL', '2')} is a placeholder for private information that stays on the user's computer.`,
  `Treat each placeholder as the real value and use it exactly as written; never guess what it stands for, and do not call the information missing.`,
  `Placeholders that share a number, such as ${eg('PERSON', '3')} and ${eg('PERSON', '3B')}, are different written forms of one name (a full name and a surname alone, say), so they very likely refer to the same person.`,
  `Text such as ${'['}literal ${eg('PERSON', '1').slice(1)} is not a placeholder: it is text that already looked like one (in a paper about redaction, say), and it means exactly the bracketed text without the word literal. Copy it as it is; Pātaka turns it back into what was written.`,
  `Pātaka puts real values back into the arguments of tools on this computer before they run. When a shell command needs real values but Claude Code refuses it because a hook changed its input (as auto mode does), run it as \`pataka run -- <command> [arguments]\` instead: the command is checked as written, and Pātaka puts the real values into its arguments only as it runs. Its arguments go to the program directly, not through a shell, so use it for one program and its arguments, without pipes or redirection.`,
].join(' ')

let base: string | undefined
let redactedCount = 0
/** Drawn reply text → the same text with real values, kept in memory only. */
const restoredText = new Map<string, string>()

async function serviceBase($: Dollar): Promise<string> {
  if (base) return base
  const home = await $.env.get('HOME')
  const candidates = [
    `${home}/Library/Application Support/nz.pataka.desktop/service.json`,
    `${home}/.local/share/nz.pataka.desktop/service.json`,
  ]
  for (const path of candidates) {
    if (await $.fs.exists(path)) {
      const c = JSON.parse(await $.fs.read(path)) as { port: number; token: string }
      base = `http://127.0.0.1:${c.port}/s/${c.token}`
      return base
    }
  }
  throw new Error('the Pātaka service is not installed (`pataka service install`)')
}

async function call<T>($: Dollar, action: string, body: unknown): Promise<T> {
  const url = `${await serviceBase($)}/pataka/${action}`
  let res
  try {
    res = await $.http.fetch(url, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    })
  } catch {
    base = undefined
    throw new Error(`the Pātaka service is not running. ${LOCKED_HINT}`)
  }
  if (!res.ok) {
    let msg = `Pātaka answered ${res.status}`
    try {
      msg = (JSON.parse(res.text) as { error?: string }).error ?? msg
    } catch {}
    throw new Error(`${msg} ${LOCKED_HINT}`)
  }
  return JSON.parse(res.text) as T
}

type Redacted = { texts: string[]; categories: Record<string, number>; tapu: boolean; citedAuthors?: number }

const SHOW = 'Show them'
const HIDE = 'Keep hidden'
const ALWAYS = 'Always show'
/** Asked at most once per load; the service keeps the answer while it runs. */
let citedAsked = false

/** The pataka command: where the installer puts it, else on the PATH. */
async function patakaCommand($: Dollar): Promise<string> {
  const home = await $.env.get('HOME')
  const local = `${home}/.local/bin/pataka`
  return (await $.fs.exists(local)) ? local : 'pataka'
}

/**
 * Ask, once, whether the authors a paper names (its byline, references and
 * citations) may go to the model. Returns whether to show them. Dismissed,
 * or with nobody to ask, they stay hidden and nothing is recorded, so a
 * later session asks again.
 */
async function askAboutCited($: Dollar, n: number): Promise<boolean> {
  if (citedAsked) return false
  citedAsked = true
  let answer: string
  try {
    answer = await $.ui.ask(
      `This text names ${n} published author${n === 1 ? '' : 's'} in a byline, reference list or citations. Pātaka hides names by default. Show these authors' names to Claude?`,
      { header: 'Citations', options: [SHOW, HIDE, ALWAYS] },
    )
  } catch {
    return false
  }
  const show = answer === SHOW || answer === ALWAYS
  await call($, 'cited-authors', { show })
  if (answer === ALWAYS) {
    const hint = 'Run `pataka lists cited-authors show` to always show them.'
    try {
      const r = await $.process.run([await patakaCommand($), 'lists', 'cited-authors', 'show'])
      if (r.exitCode !== 0) $.ui.toast(`Tūmataiti: shown for now, but the setting was not saved. ${hint}`)
    } catch {
      $.ui.toast(`Tūmataiti: shown for now, but the setting was not saved. ${hint}`)
    }
  }
  return show
}

async function redactTexts($: Dollar, texts: string[]): Promise<Redacted> {
  if (texts.every(t => !t.trim())) return { texts, categories: {}, tapu: false }
  const r = await call<Redacted>($, 'redact', { texts })
  if (!Array.isArray(r.texts) || r.texts.length !== texts.length) throw new Error('Pātaka gave an unexpected answer')
  const n = Object.values(r.categories).reduce((a, b) => a + b, 0)
  if (n > 0) {
    redactedCount += n
    $.ui.status(`Tūmataiti: ${redactedCount} item${redactedCount === 1 ? '' : 's'} kept on this computer`)
  }
  return r
}

/** Every string a block carries as text (`text`, a tool result's `content`), with a setter. */
function textSlots(blocks: Block[], out: { get: () => string; set: (s: string) => void }[]): void {
  for (const b of blocks) {
    for (const key of ['text', 'content'] as const) {
      const v = b[key]
      if (typeof v === 'string') out.push({ get: () => b[key] as string, set: s => { b[key] = s } })
      else if (Array.isArray(v)) textSlots(v as Block[], out)
    }
  }
}

/**
 * Images cannot be checked, so they are replaced with a note. A PDF becomes
 * its text, read on this computer by the Pātaka service (pictures in it are
 * not read), and a plain-text document becomes its text: both are then
 * redacted with the rest of the row.
 */
async function readableMedia($: Dollar, blocks: Block[]): Promise<Block[]> {
  const out: Block[] = []
  for (const b of blocks) {
    if (b.type === 'image') out.push({ type: 'text', text: MEDIA })
    else if (b.type === 'document') out.push({ type: 'text', text: await documentText($, b) })
    else if (Array.isArray(b.content)) out.push({ ...b, content: await readableMedia($, b.content as Block[]) })
    else out.push({ ...b })
  }
  return out
}

/** A document block's text, or a note saying why it was withheld. */
async function documentText($: Dollar, b: Block): Promise<string> {
  const src = (b.source ?? {}) as { type?: string; media_type?: string; data?: unknown }
  const title = typeof b.title === 'string' ? b.title : undefined
  if (src.type === 'text' && typeof src.data === 'string') {
    return `[document${title ? ` "${title}"` : ''}]\n${src.data}`
  }
  if (src.type === 'base64' && src.media_type === 'application/pdf' && typeof src.data === 'string') {
    try {
      const r = await call<{ text?: unknown }>($, 'pdf-text', { data: src.data, title })
      if (typeof r.text === 'string') return r.text
    } catch (err) {
      const why = (err as Error).message.replace(LOCKED_HINT, '').trim()
      return `[document withheld by Pātaka: ${why}]`
    }
  }
  return MEDIA
}

/** What a row says when it could not be checked: tool results keep their ids. */
function withheld(blocks: Block[]): Block[] {
  return blocks.map(b =>
    b.type === 'tool_result'
      ? { type: 'tool_result', tool_use_id: b.tool_use_id, content: WITHHELD, is_error: true }
      : { type: 'text', text: WITHHELD },
  )
}

export const register: Register = on => {
  // A typed prompt: redacted before it is shown, stored or sent. Tapu terms
  // stop it. If Pātaka cannot check it, it is not sent.
  on('prompt.submit', async ($, e, next) => {
    if (e.command) return next(e)
    const r = await redactTexts($, [e.text])
    if (r.tapu) return { drop: 'Pātaka stopped this prompt: it contains a term your kaupapa marks tapu. Nothing was sent.' }
    return next({ ...e, text: r.texts[0] ?? '' })
  }).catch(($, e, next) =>
    next.called
      ? next(e)
      : { drop: `Pātaka could not check this prompt, so nothing was sent. ${LOCKED_HINT}` },
  )

  // Every other row that enters the conversation: tool results, attachments,
  // injected context. Only the redacted row is stored, so only it is sent.
  on('session.append', async ($, e, next) => {
    if (SKIP_DOORS.has(e.door) || e.message.type === 'system') return next(e)
    const content = await readableMedia($, e.message.content as Block[])
    const slots: { get: () => string; set: (s: string) => void }[] = []
    textSlots(content, slots)
    if (slots.length === 0) return next({ ...e, message: { ...e.message, content } })
    const originals = slots.map(s => s.get())
    let r = await redactTexts($, originals)
    // Cited authors held back: ask once, and if they may go, check again.
    if (!r.tapu && (r.citedAuthors ?? 0) > 0 && (await askAboutCited($, r.citedAuthors ?? 0))) {
      r = await redactTexts($, originals)
    }
    if (r.tapu) {
      return next({ ...e, message: { ...e.message, content: withheld(content) } })
    }
    slots.forEach((s, i) => s.set(r.texts[i] ?? WITHHELD))
    return next({ ...e, message: { ...e.message, content } })
  }).catch(($, e, next) =>
    next.called
      ? next(e)
      : next({ ...e, message: { ...e.message, content: withheld(e.message.content as Block[]) } }),
  )

  // Before a local tool runs, its arguments get the real values back. Network
  // and MCP tools keep the placeholders (Pātaka Mail restores its own).
  on('tool.call', async ($, e, next) => {
    if (!restoresInput(e.tool)) return next(e)
    const args: Record<string, unknown> = {}
    for (const [k, v] of Object.entries(e)) if (!RESERVED.has(k)) args[k] = v
    // Left exactly as written, so auto mode reviews what runs.
    if (e.tool === 'Bash' && runsThroughPataka(args)) return next(e)
    if (!PLACEHOLDER.test(JSON.stringify(args))) return next(e)
    const { value } = await call<{ value: Record<string, Json> }>($, 'restore', { value: args })
    return next({ ...e, ...value } as typeof e)
  }).catch(($, e, next) =>
    next.called
      ? next(e)
      : { deny: `Pātaka could not put the real values back into this ${e.tool} call, so it did not run. ${LOCKED_HINT}` },
  )

  // The model only ever writes placeholders, so its replies are stored with
  // them (the `response` door is skipped above). Put the real values back in
  // what the person sees: this rewrites the drawing alone, never the stored
  // row, so the model still reads placeholders next turn. If Pātaka cannot
  // restore (locked, not running), the reply is drawn as stored.
  on('ui.render', { component: 'AssistantMessage' }, async ($, e, next) => {
    const text = e.props.text
    if (!PLACEHOLDER.test(text)) return next(e)
    let shown = restoredText.get(text)
    if (shown === undefined) {
      const { value } = await call<{ value: Json }>($, 'restore', { value: text })
      if (typeof value !== 'string') return next(e)
      shown = value
      if (restoredText.size >= 500) restoredText.clear()
      restoredText.set(text, shown)
    }
    return next({ ...e, props: { ...e.props, text: shown } })
  }).catch(($, e, next) => next(e))

  // Tell the model what placeholders are, and how to run a command that
  // needs real values without a hook changing it.
  on('prompt.compose', async ($, e, next) => {
    const out = await next(e)
    return { ...out, sections: [...out.sections, { id: 'tumataiti:placeholders', text: NOTE, scope: 'session' }] }
  }).catch(($, e, next) => next(e))

  on('session.start', async ($, e, next) => {
    const out = await next(e)
    try {
      await serviceBase($)
      $.ui.status('Tūmataiti: on')
    } catch (err) {
      $.ui.status('Tūmataiti: service not found')
      $.ui.toast(`Tūmataiti: ${(err as Error).message}`)
    }
    return out
  })
}
