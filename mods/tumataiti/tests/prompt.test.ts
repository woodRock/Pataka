import { expect, mock, test } from 'claude-code/testing'

// Placeholders are built at run time, so this file never holds one literally.
const ph = (label: string, n: number) => `[${label}_${n}]`
const PERSON = ph('PERSON', 1)

type Fetch = { url: string; init: { body: string } }
type Row = { type: string; text?: string }
type On = Parameters<Parameters<typeof test>[1]>[1]

/** The Pātaka service: its redact marks placeholder-like text `literal`, as the real one does. */
function fakeService(on: On) {
  const calls: string[][] = []
  on('env.get', () => ({ value: '/home/t' }))
  on('fs.exists', () => ({ value: true }))
  on('fs.read', () => ({ value: JSON.stringify({ port: 1, token: 't' }) }))
  on('http.fetch', (_, e) => {
    const { init } = e as unknown as Fetch
    const body = JSON.parse(init.body) as { texts?: string[] }
    calls.push(body.texts ?? [])
    const texts = (body.texts ?? []).map(t => t.replace(/\[([A-Z]+_\d+)\]/g, '[literal $1]'))
    return { value: { ok: true, status: 200, text: JSON.stringify({ texts, categories: {}, tapu: false, citedAuthors: 0 }) } }
  })
  return calls
}

const row = (door: 'prompt' | 'hook-context', text: string, uuid: string) => ({
  message: { type: 'user' as const, role: 'user' as const, content: [{ type: 'text', text }] },
  door,
  origin: { kind: 'user' as const },
  uuid,
})

const storedText = (session: ReturnType<typeof mock.session>, i: number) =>
  (session.appended()[i]?.message.content as Row[])[0]?.text

test('a typed prompt keeps the placeholders prompt.submit gave it; other rows are still escaped', async ($, on) => {
  const calls = fakeService(on)
  const session = mock.session(on)
  await $.session.append(row('prompt', `email ${PERSON}`, 'p1') as never)
  await $.session.append(row('hook-context', `notes: ${PERSON}`, 'h1') as never)
  expect(storedText(session, 0)).toBe(`email ${PERSON}`)
  expect(storedText(session, 1)).toBe(`notes: [literal ${PERSON.slice(1)}`)
  // The prompt's text never went to the service to be redacted again.
  expect(calls).toEqual([[`notes: ${PERSON}`]])
})
