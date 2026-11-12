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
 * Pātaka Mail's tools (`pataka claude` with a connected mailbox). They search
 * the person's own mail server, so they need real names too, but only in a
 * session `pataka claude` started with mail registered: it sets
 * PATAKA_MAIL_URL to its proxy on 127.0.0.1. Every other MCP tool keeps its
 * placeholders.
 */
const MAIL_TOOL = /^mcp__pataka-mail__[A-Za-z0-9_]+$/
let mailSession: boolean | undefined

async function restoresInput($: Dollar, tool: string): Promise<boolean> {
  if (LOCAL_TOOLS.has(tool)) return true
  if (!MAIL_TOOL.test(tool)) return false
  if (mailSession === undefined) {
    const url = (await $.env.get('PATAKA_MAIL_URL')) ?? ''
    mailSession = url.startsWith('http://127.0.0.1:') && url.endsWith('/pataka/mcp')
  }
  return mailSession
}

/** Keys of tool.call's input that are not the tool's own arguments. */
const RESERVED = new Set(['tool', 'tool_use_id', 'agentId', 'requestMeta', 'consent'])
/** Rows the model never reads, or that it wrote itself from placeholders. */
const SKIP_DOORS = new Set(['response', 'compaction', 'notice'])
const PLACEHOLDER = /\[[A-Z][A-Z_]*_\d+\]/

let base: string | undefined
let redactedCount = 0

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

type Redacted = { texts: string[]; categories: Record<string, number>; tapu: boolean }

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

/** Images and documents cannot be checked, so they are replaced with a note. */
function withoutMedia(blocks: Block[]): Block[] {
  return blocks.map(b => {
    if (b.type === 'image' || b.type === 'document') return { type: 'text', text: MEDIA }
    if (Array.isArray(b.content)) return { ...b, content: withoutMedia(b.content as Block[]) }
    return { ...b }
  })
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
    const content = withoutMedia(e.message.content as Block[])
    const slots: { get: () => string; set: (s: string) => void }[] = []
    textSlots(content, slots)
    if (slots.length === 0) return next({ ...e, message: { ...e.message, content } })
    const r = await redactTexts($, slots.map(s => s.get()))
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

  // Before a local tool runs, its arguments get the real values back, and so
  // do Pātaka Mail's under `pataka claude`. Network and other MCP tools keep
  // the placeholders.
  on('tool.call', async ($, e, next) => {
    if (!(await restoresInput($, e.tool))) return next(e)
    const args: Record<string, unknown> = {}
    for (const [k, v] of Object.entries(e)) if (!RESERVED.has(k)) args[k] = v
    if (!PLACEHOLDER.test(JSON.stringify(args))) return next(e)
    const { value } = await call<{ value: Record<string, Json> }>($, 'restore', { value: args })
    return next({ ...e, ...value } as typeof e)
  }).catch(($, e, next) =>
    next.called
      ? next(e)
      : { deny: `Pātaka could not put the real values back into this ${e.tool} call, so it did not run. ${LOCKED_HINT}` },
  )

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
