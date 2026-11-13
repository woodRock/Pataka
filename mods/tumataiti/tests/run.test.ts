import { expect, test } from 'claude-code/testing'

// Placeholders are built at run time, so this file never holds one literally.
const ph = (label: string, n: string) => `[${label}_${n}]`
const PERSON = ph('PERSON', '1')
const PERSON_FORM = ph('PERSON', '1B')

function fakeService(on: Parameters<Parameters<typeof test>[1]>[1], calls: string[]) {
  on('env.get', () => ({ value: '/home/t' }))
  on('fs.exists', () => ({ value: true }))
  on('fs.read', () => ({ value: JSON.stringify({ port: 1, token: 't' }) }))
  on('http.fetch', (_, e) => {
    const body = JSON.parse((e as { init: { body: string } }).init.body) as { value: unknown }
    calls.push(JSON.stringify(body.value))
    const out = JSON.parse(JSON.stringify(body.value).split(PERSON_FORM).join('Kereama').split(PERSON).join('Aroha Kereama'))
    return { value: { ok: true, status: 200, text: JSON.stringify({ value: out }) } }
  })
}

/** Records the command each Bash call arrives with beneath the mod. */
function bashSeen(on: Parameters<Parameters<typeof test>[1]>[1], seen: string[]) {
  on('tool.call', { tool: 'Bash' }, (_, e) => {
    seen.push((e as { command: string }).command)
    return { result: '', text: 'ok' }
  })
}

test('a command run through pataka run reaches Bash as written', async ($, on) => {
  const calls: string[] = []
  const seen: string[] = []
  fakeService(on, calls)
  bashSeen(on, seen)
  const command = `pataka run -- ruru verdict abd7e9a108 --quote "${PERSON} wrote"`
  await $.tool.call({ tool: 'Bash', command })
  expect(seen).toEqual([command])
  expect(calls.length).toBe(0)
})

test('other commands still get real values, lettered forms too', async ($, on) => {
  const calls: string[] = []
  const seen: string[] = []
  fakeService(on, calls)
  bashSeen(on, seen)
  await $.tool.call({ tool: 'Bash', command: `echo ${PERSON} ${PERSON_FORM}` })
  expect(seen).toEqual(['echo Aroha Kereama Kereama'])
  // A word that only contains "pataka run" is not the command.
  await $.tool.call({ tool: 'Bash', command: `echo mypataka run ${PERSON_FORM}` })
  expect(seen[1]).toBe('echo mypataka run Kereama')
})

test('the model is told about placeholders and pataka run', async ($, on) => {
  on('prompt.compose', () => ({ sections: [{ id: 'engine', text: 'base', scope: 'shared' }] }))
  const out = await $.prompt.compose({ promptModel: 'claude-opus-5-5', surfaces: ['desktop'], tools: ['Bash'], outputStyle: null, traits: [] } as never)
  const note = out.sections.find(s => s.id === 'tumataiti:placeholders')
  expect(note?.scope).toBe('session')
  expect(note?.text.includes('pataka run --')).toBe(true)
  expect(note?.text.includes('share a number')).toBe(true)
})

test('an escaped literal in a command is sent back to be unescaped', async ($, on) => {
  const calls: string[] = []
  const seen: string[] = []
  fakeService(on, calls)
  bashSeen(on, seen)
  const literal = `[literal ${PERSON.slice(1)}`
  await $.tool.call({ tool: 'Bash', command: `echo ${literal}` })
  expect(calls.length).toBe(1)
})
