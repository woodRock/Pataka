import type { EngineInterface, RenderInput } from 'claude-code'
import { expect, test } from 'claude-code/testing'

// Placeholders are built at run time, so this file never holds one literally.
const ph = (label: string, n: number) => `[${label}_${n}]`
const PERSON = ph('PERSON', 1)

function fakeService(on: Parameters<Parameters<typeof test>[1]>[1], restore: (v: string) => string | null) {
  on('env.get', () => ({ value: '/home/t' }))
  on('fs.exists', () => ({ value: true }))
  on('fs.read', () => ({ value: JSON.stringify({ port: 1, token: 't' }) }))
  on('http.fetch', (_, e) => {
    const body = JSON.parse((e as { init: { body: string } }).init.body) as { value: string }
    const out = restore(body.value)
    return out === null
      ? { value: { ok: false, status: 403, text: JSON.stringify({ error: 'Pātaka is locked.' }) } }
      : { value: { ok: true, status: 200, text: JSON.stringify({ value: out }) } }
  })
}

/** Stands for the engine's own drawing of the reply. */
const engine = ($: EngineInterface, e: RenderInput) => {
  const { Text } = $.ui.resolve(e)
  return <Text>{(e.props as { text: string }).text}</Text>
}

const reply = (text: string) => ({
  plugin: 'tumataiti',
  component: 'AssistantMessage',
  requestId: 'm1',
  surface: 'desktop',
  props: { text, isFirstOfReply: true },
}) as const

test('a reply is drawn with real values', async ($, on) => {
  fakeService(on, v => v.split(PERSON).join('Aroha'))
  on('ui.render', engine)
  const ui = await $.ui.mount(reply(`Unread from ${PERSON}.`))
  expect((await ui.find({ type: 'Text' }))?.text).toBe('Unread from Aroha.')
  await ui.unmount()
})

test('a reply with no placeholder is left alone', async ($, on) => {
  let fetched = false
  fakeService(on, v => ((fetched = true), v))
  on('ui.render', engine)
  const ui = await $.ui.mount(reply('Nothing personal.'))
  expect((await ui.find({ type: 'Text' }))?.text).toBe('Nothing personal.')
  expect(fetched).toBe(false)
  await ui.unmount()
})

test('a locked service draws the reply as stored', async ($, on) => {
  fakeService(on, () => null)
  on('ui.render', engine)
  const ui = await $.ui.mount(reply(`From ${PERSON}.`))
  expect((await ui.find({ type: 'Text' }))?.text).toBe(`From ${PERSON}.`)
  await ui.unmount()
})
