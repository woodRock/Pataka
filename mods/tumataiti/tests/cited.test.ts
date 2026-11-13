import { expect, mock, test } from 'claude-code/testing'

// Placeholders are built at run time, so this file never holds one literally.
const ph = (label: string, n: number) => `[${label}_${n}]`
const PERSON = ph('PERSON', 1)

type Fetch = { url: string; init: { body: string } }
type Row = { type: string; text?: string; content?: Row[] }
type On = Parameters<Parameters<typeof test>[1]>[1]

/**
 * The Pātaka service: "Sweeney" is a cited author it holds back until told
 * to show cited authors.
 */
function fakeService(on: On) {
  const calls: string[] = []
  let shown = false
  on('env.get', () => ({ value: '/home/t' }))
  on('fs.exists', () => ({ value: true }))
  on('fs.read', () => ({ value: JSON.stringify({ port: 1, token: 't' }) }))
  on('http.fetch', (_, e) => {
    const { url, init } = e as unknown as Fetch
    const action = url.split('/').pop() ?? ''
    const body = JSON.parse(init.body) as { texts?: string[]; show?: boolean }
    calls.push(action === 'cited-authors' ? `${action}:${body.show}` : action)
    if (action === 'cited-authors') {
      shown = body.show === true
      return { value: { ok: true, status: 200, text: JSON.stringify({ show: shown }) } }
    }
    const texts = (body.texts ?? []).map(t => (shown ? t : t.split('Sweeney').join(PERSON)))
    const cited = !shown && (body.texts ?? []).some(t => t.includes('Sweeney')) ? 1 : 0
    return { value: { ok: true, status: 200, text: JSON.stringify({ texts, categories: {}, tapu: false, citedAuthors: cited }) } }
  })
  return calls
}

/** Answer Pātaka's question with `label`, or dismiss it (null). */
function answer(on: On, label: string | null) {
  const asked: string[] = []
  on('tool.call', { tool: 'AskUserQuestion' }, (_, e) => {
    const q = (e as unknown as { questions: { question: string }[] }).questions[0]?.question ?? ''
    asked.push(q)
    if (label === null) return { result: { questions: [], answers: {} }, text: '', isError: true }
    return { result: { questions: [], answers: { [q]: label } }, text: '' }
  })
  return asked
}

const row = (text: string, uuid: string) => ({
  message: { type: 'user' as const, role: 'user' as const, content: [{ type: 'tool_result', tool_use_id: uuid, content: [{ type: 'text', text }] }] },
  door: 'tool-result' as const,
  origin: { kind: 'tool' as const, tool: 'Read' },
  uuid,
})

const storedText = (session: ReturnType<typeof mock.session>, i: number) =>
  ((session.appended()[i]?.message.content as Row[])[0]?.content ?? [])[0]?.text

test('shown cited authors are stored in the clear, after one question', async ($, on) => {
  const calls = fakeService(on)
  const asked = answer(on, 'Show them')
  const session = mock.session(on)
  await $.session.append(row('As shown (Sweeney, 2002).', 'u1') as never)
  await $.session.append(row('Sweeney again.', 'u2') as never)
  expect(storedText(session, 0)).toBe('As shown (Sweeney, 2002).')
  expect(storedText(session, 1)).toBe('Sweeney again.')
  expect(asked.length).toBe(1)
  expect(asked[0]).toContain('published author')
  expect(calls).toEqual(['redact', 'cited-authors:true', 'redact', 'redact'])
})

test('kept hidden, they stay as placeholders and are not asked about again', async ($, on) => {
  const calls = fakeService(on)
  const asked = answer(on, 'Keep hidden')
  const session = mock.session(on)
  await $.session.append(row('As shown (Sweeney, 2002).', 'u1') as never)
  await $.session.append(row('Sweeney again.', 'u2') as never)
  expect(storedText(session, 0)).toBe(`As shown (${PERSON}, 2002).`)
  expect(asked.length).toBe(1)
  expect(calls).toEqual(['redact', 'cited-authors:false', 'redact'])
})

test('a dismissed question keeps them hidden and records nothing', async ($, on) => {
  const calls = fakeService(on)
  answer(on, null)
  const session = mock.session(on)
  await $.session.append(row('As shown (Sweeney, 2002).', 'u1') as never)
  expect(storedText(session, 0)).toBe(`As shown (${PERSON}, 2002).`)
  expect(calls).toEqual(['redact'])
})
