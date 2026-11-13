import { expect, mock, test } from 'claude-code/testing'

// Placeholders are built at run time, so this file never holds one literally.
const ph = (label: string, n: number) => `[${label}_${n}]`
const EMAIL = ph('EMAIL', 1)

type Fetch = { url: string; init: { body: string } }
type Row = { type: string; text?: string; content?: Row[] }

/** The Pātaka service: reads a PDF's text, and redacts one made-up address. */
function fakeService(on: Parameters<Parameters<typeof test>[1]>[1], pdf: 'ok' | 'scanned') {
  const calls: string[] = []
  on('env.get', () => ({ value: '/home/t' }))
  on('fs.exists', () => ({ value: true }))
  on('fs.read', () => ({ value: JSON.stringify({ port: 1, token: 't' }) }))
  on('http.fetch', (_, e) => {
    const { url, init } = e as unknown as Fetch
    const action = url.split('/').pop() ?? ''
    calls.push(action)
    const body = JSON.parse(init.body) as { texts?: string[]; data?: string }
    if (action === 'pdf-text') {
      return pdf === 'ok'
        ? { value: { ok: true, status: 200, text: JSON.stringify({ text: '[PDF "paper.pdf", 1 page(s)]\nWrite to kiri@kowhai.nz' }) } }
        : { value: { ok: false, status: 422, text: JSON.stringify({ error: 'This PDF has no text to read.' }) } }
    }
    const texts = (body.texts ?? []).map(t => t.split('kiri@kowhai.nz').join(EMAIL))
    return { value: { ok: true, status: 200, text: JSON.stringify({ texts, categories: {}, tapu: false }) } }
  })
  return calls
}

const row = (content: unknown[]) => ({
  message: {
    type: 'user' as const,
    role: 'user' as const,
    content: [{ type: 'tool_result', tool_use_id: 't1', content }],
  },
  door: 'tool-result' as const,
  origin: { kind: 'tool' as const, tool: 'Read' },
  uuid: 'u1',
})

const stored = (session: ReturnType<typeof mock.session>) =>
  ((session.appended()[0]?.message.content as Row[])[0]?.content ?? []) as Row[]

test('a PDF is stored as its redacted text, and an image is still withheld', async ($, on) => {
  const calls = fakeService(on, 'ok')
  const session = mock.session(on)
  await $.session.append(row([
    { type: 'document', title: 'paper.pdf', source: { type: 'base64', media_type: 'application/pdf', data: 'JVBERi0=' } },
    { type: 'image', source: { type: 'base64', media_type: 'image/png', data: 'AAAA' } },
  ]) as never)
  const [pdf, image] = stored(session)
  expect(pdf?.type).toBe('text')
  expect(pdf?.text).toBe(`[PDF "paper.pdf", 1 page(s)]\nWrite to ${EMAIL}`)
  expect(image?.text).toContain('withheld by Pātaka')
  expect(JSON.stringify(session.appended())).not.toContain('kowhai')
  expect(JSON.stringify(session.appended())).not.toContain('AAAA')
  expect(calls).toEqual(['pdf-text', 'redact'])
})

test('a PDF Pātaka cannot read is withheld, saying why', async ($, on) => {
  fakeService(on, 'scanned')
  const session = mock.session(on)
  await $.session.append(row([
    { type: 'document', source: { type: 'base64', media_type: 'application/pdf', data: 'JVBERi0=' } },
  ]) as never)
  const [pdf] = stored(session)
  expect(pdf?.text).toBe('[document withheld by Pātaka: This PDF has no text to read.]')
  expect(JSON.stringify(session.appended())).not.toContain('JVBERi0')
})
