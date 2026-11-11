// Pātaka window logic. Everything sensitive (keys, network, storage,
// redaction) happens in Rust; this file only renders and calls commands.
import { renderMarkdown } from './markdown.js';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const el = (tag, props = {}, ...kids) => {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (k === 'class') e.className = v;
    else if (k === 'text') e.textContent = v;
    else if (k.startsWith('on')) e.addEventListener(k.slice(2), v);
    else if (v !== undefined && v !== null && v !== false) e.setAttribute(k, v === true ? '' : v);
  }
  for (const kid of kids) if (kid !== null && kid !== undefined) e.append(kid);
  return e;
};

const CATEGORIES = [
  ['PERSON', 'Person'], ['ADDRESS', 'Address'], ['EMAIL', 'Email'], ['PHONE', 'Phone'], ['URL', 'URL'],
  ['IP_ADDRESS', 'IP address'], ['DATE', 'Date (birth/death)'], ['NZ_GOV_ID', 'NZ government ID'],
  ['FINANCIAL', 'Bank account / card'], ['OTHER_ID', 'Other ID'], ['SECRET', 'Password / API key'],
  ['AFFILIATION', 'Affiliation (iwi, hapū, marae)'], ['HEALTH', 'Health'], ['CULTURAL_RESTRICTED', 'Tapu / restricted'],
];
const ACTIONS = ['allow', 'flag', 'pseudonymise', 'generalise', 'mask', 'block'];
const LOCKED_ACTIONS = { SECRET: 'block', CULTURAL_RESTRICTED: 'block' };
const TRUST_TEXT = {
  device: 'On this device',
  sovereign: 'Sovereign (you control it)',
  offshore: 'Leaves the device',
};

const state = {
  settings: null,
  list: [],
  current: null, // ConversationView
  streaming: new Set(),
  lastActivity: Date.now(),
  model: null, // ModelStatus
  // Show real values in place of placeholders. A per-device display
  // preference only; it holds no kōrero content.
  reveal: (() => { try { return localStorage.getItem('pataka.reveal') === '1'; } catch { return false; } })(),
};

// ------------------------------------------------------------ utilities

function toast(msg, isErr = false) {
  const t = $('toast');
  t.textContent = msg;
  t.classList.toggle('err', isErr);
  t.classList.remove('hidden');
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => t.classList.add('hidden'), isErr ? 7000 : 3500);
}

async function call(cmd, args = {}) {
  try {
    return await invoke(cmd, args);
  } catch (e) {
    const msg = typeof e === 'string' ? e : (e && e.message) || 'Something went wrong.';
    if (msg.startsWith('Pātaka is locked')) showLock();
    throw new Error(msg);
  }
}

async function attempt(fn) {
  try {
    return await fn();
  } catch (e) {
    toast(e.message, true);
    return undefined;
  }
}

function fmtTime(secs) {
  return new Date(secs * 1000).toLocaleString();
}

function foundBy(f) {
  if (f.source === 'model') return `name model, ${Math.round(f.confidence * 100)}%`;
  if (f.source === 'gazetteer') return f.detector === 'tapu_term' ? 'tapu list' : f.detector === 'korero_term' ? 'this kōrero' : 'kaitiaki list';
  return `rule (${f.detector})`;
}

function catLabel(code) {
  const c = CATEGORIES.find(([k]) => k === code);
  return c ? c[1] : code;
}

function highlightPlaceholders(text) {
  const frag = document.createDocumentFragment();
  const re = /\[(?:[A-Z_]+(?:_\d+)?|YEAR \d{4})\]/g;
  let last = 0;
  let m;
  while ((m = re.exec(text))) {
    if (m.index > last) frag.append(text.slice(last, m.index));
    frag.append(el('mark', { text: m[0] }));
    last = m.index + m[0].length;
  }
  frag.append(text.slice(last));
  return frag;
}

// Placeholders in the chat are shown as chips, so it stays clear what the
// model actually received. Hovering or focusing a chip shows the other form
// (the real value, or the placeholder when real values are shown); clicking
// swaps that one chip. Real values come from the kōrero's own vault and never
// leave the device.
const PLACEHOLDER_RE = /\[(?:[A-Z_]+(?:_\d+)?|YEAR \d{4})\]/g;

function placeholderChip(ph, original, reveal) {
  const known = typeof original === 'string';
  let showing = known && reveal;
  const chip = el('button', { type: 'button', class: known ? 'ph' : 'ph masked' });
  const paint = () => {
    chip.textContent = showing ? original : ph;
    chip.classList.toggle('revealed', showing);
    const tip = !known
      ? 'Replaced before sending. Pātaka keeps no copy of the original.'
      : showing
        ? `Kept on this device. The model saw ${ph}.`
        : `${original}: shown here only. The model saw ${ph}.`;
    chip.dataset.tip = tip;
    chip.setAttribute('aria-label', known ? `${showing ? original : ph}. ${tip}` : `${ph}. ${tip}`);
  };
  if (known) chip.addEventListener('click', () => { showing = !showing; paint(); });
  paint();
  return chip;
}

function chipText(redactions) {
  const map = redactions || {};
  return (parent, text) => {
    let last = 0;
    PLACEHOLDER_RE.lastIndex = 0;
    let m;
    while ((m = PLACEHOLDER_RE.exec(text))) {
      if (m.index > last) parent.append(text.slice(last, m.index));
      const original = Object.prototype.hasOwnProperty.call(map, m[0]) ? map[m[0]] : undefined;
      parent.append(placeholderChip(m[0], original, state.reveal));
      last = m.index + m[0].length;
    }
    if (last < text.length) parent.append(text.slice(last));
  };
}

function countPlaceholders(text) {
  return (text.match(PLACEHOLDER_RE) || []).length;
}

function updateRevealToggle() {
  const c = state.current;
  const b = $('reveal-toggle');
  const any = !!c && c.messages.some((m) => countPlaceholders(m.role === 'user' ? (m.outbound || '') : (m.text || '')) > 0);
  b.classList.toggle('hidden', !any);
  b.textContent = state.reveal ? 'Show placeholders' : 'Show real values';
  b.setAttribute('aria-pressed', String(state.reveal));
}

$('reveal-toggle').addEventListener('click', () => {
  state.reveal = !state.reveal;
  try { localStorage.setItem('pataka.reveal', state.reveal ? '1' : '0'); } catch { /* display preference only */ }
  renderKorero();
});

// ------------------------------------------------------------ lock screen

async function showLock() {
  $('app').classList.add('hidden');
  $('lock').classList.remove('hidden');
  for (const d of document.querySelectorAll('dialog[open]')) d.close();
  $('lock-error').textContent = '';
  const status = await invoke('vault_status');
  $('setup-form').classList.toggle('hidden', status.initialised);
  $('unlock-form').classList.toggle('hidden', !status.initialised);
  if (status.initialised) {
    const pass = status.mode === 'passphrase';
    $('unlock-pass-label').classList.toggle('hidden', !pass);
    $('unlock-pass').value = '';
    if (pass) $('unlock-pass').focus();
  }
}

function syncSetupMode() {
  const mode = document.querySelector('input[name=mode]:checked').value;
  $('setup-pass').classList.toggle('hidden', mode !== 'passphrase');
}

$('setup-form').addEventListener('change', syncSetupMode);
$('setup-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const mode = document.querySelector('input[name=mode]:checked').value;
  const p1 = $('setup-pass1').value;
  if (mode === 'passphrase' && p1 !== $('setup-pass2').value) {
    $('lock-error').textContent = 'The passphrases do not match.';
    return;
  }
  $('lock-error').textContent = mode === 'passphrase' ? 'Deriving a key from your passphrase…' : '';
  try {
    await call('vault_setup', { mode, passphrase: mode === 'passphrase' ? p1 : null });
    $('setup-pass1').value = '';
    $('setup-pass2').value = '';
    await enterApp();
  } catch (err) {
    $('lock-error').textContent = err.message;
  }
});

$('unlock-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  $('lock-error').textContent = 'Unlocking…';
  try {
    const pass = $('unlock-pass').value;
    $('unlock-pass').value = '';
    await call('vault_unlock', { passphrase: pass || null });
    await enterApp();
  } catch (err) {
    $('lock-error').textContent = err.message;
  }
});

async function enterApp() {
  $('lock').classList.add('hidden');
  $('app').classList.remove('hidden');
  state.lastActivity = Date.now();
  state.settings = await call('get_settings');
  state.model = await call('model_status');
  fillProviderSelect();
  await refreshList();
  if (state.list.length) await openKorero(state.list[0].id);
  else showEmpty();
}

// ------------------------------------------------------------ sidebar

async function refreshList() {
  state.list = await call('list_conversations');
  const nav = $('korero-list');
  nav.replaceChildren();
  for (const k of state.list) {
    const item = el('button', {
      class: 'korero-item' + (state.current && state.current.id === k.id ? ' active' : ''),
      title: k.title,
      onclick: () => attempt(() => openKorero(k.id)),
    }, el('span', { class: 't', text: k.title }), k.classification === 'tapu' ? el('span', { class: 'pill tapu', text: 'tapu' }) : null);
    nav.append(item);
  }
}

async function newKorero(classification) {
  const c = await call('create_conversation', { classification });
  await refreshList();
  await openKorero(c.id);
  $('input').focus();
}

$('new-noa').addEventListener('click', () => attempt(() => newKorero('noa')));
$('new-tapu').addEventListener('click', () => attempt(() => newKorero('tapu')));
$('lock-now').addEventListener('click', () => attempt(lockNow));

async function lockNow() {
  await call('vault_lock');
  state.current = null;
  state.settings = null;
  state.streaming.clear();
  $('messages').replaceChildren();
  await showLock();
}

// ------------------------------------------------------------ chat

function showEmpty() {
  state.current = null;
  $('messages').replaceChildren();
  $('messages').classList.add('hidden');
  $('empty').classList.remove('hidden');
  $('korero-title').value = '';
  $('korero-title').disabled = true;
  for (const b of document.querySelectorAll('.class-toggle button')) b.classList.remove('on');
  updateRoute();
}

async function openKorero(id) {
  state.current = await call('get_conversation', { id });
  if (state.current.streaming) state.streaming.add(id);
  renderKorero();
  await refreshList();
}

function renderKorero() {
  const c = state.current;
  if (!c) return showEmpty();
  $('empty').classList.toggle('hidden', c.messages.length > 0);
  $('messages').classList.toggle('hidden', c.messages.length === 0);
  $('korero-title').disabled = false;
  $('korero-title').value = c.title;
  for (const b of document.querySelectorAll('.class-toggle button')) b.classList.toggle('on', b.dataset.cls === c.classification);
  const box = $('messages');
  box.replaceChildren(...c.messages.map(renderMessage));
  updateRevealToggle();
  box.scrollTop = box.scrollHeight;
  updateRoute();
  updateStreamingUi();
}

function routeMeta(m) {
  const parts = [];
  if (m.providerName) parts.push(m.providerName);
  if (m.model) parts.push(m.model);
  if (m.jurisdiction && m.trust !== 'device') parts.push(m.jurisdiction);
  return parts.join(' · ');
}

function renderMessage(m) {
  const wrap = el('div', { class: `msg ${m.role}`, 'data-id': m.id });
  const put = chipText(state.current?.redactions);
  if (m.role === 'user') {
    // A redacted message is shown as it was sent, with placeholders as chips.
    const sent = m.outbound !== null && m.outbound !== undefined && m.outbound !== m.text;
    const bubble = el('div', { class: 'bubble' });
    if (sent) put(bubble, m.outbound);
    else bubble.textContent = m.display;
    wrap.append(bubble);
    const meta = el('div', { class: 'meta' });
    if (m.trust) meta.append(el('span', { text: `→ ${routeMeta(m)} · ${TRUST_TEXT[m.trust] || m.trust}` }));
    const n = sent ? countPlaceholders(m.outbound) : 0;
    if (n) meta.append(el('span', { class: 'redacted-note', text: `${n} item${n === 1 ? '' : 's'} replaced before sending` }));
    wrap.append(meta);
  } else {
    const bubble = el('div', { class: 'bubble' });
    const streaming = state.streaming.has(state.current?.id) && !m.text && !m.error && isLast(m);
    if (m.text) bubble.append(renderMarkdown(m.text, put));
    else if (streaming) bubble.append(el('span', { class: 'muted typing', text: 'Thinking' }));
    else if (!m.error) bubble.append(el('span', { class: 'muted', text: '(no reply)' }));
    wrap.append(bubble);
    if (m.notice) wrap.append(el('div', { class: 'notice', text: m.notice }));
    if (m.error) wrap.append(el('div', { class: 'err', text: m.error }));
    const meta = el('div', { class: 'meta' }, el('span', { text: routeMeta(m) }));
    if (countPlaceholders(m.text || '')) {
      meta.append(el('span', {
        class: 'redacted-note',
        text: state.reveal
          ? 'Highlighted values were placeholders in the reply, filled in on this device: hover one to see what the model saw'
          : 'The model replied with placeholders: hover one to see who or what it stands for',
      }));
    }
    wrap.append(meta);
  }
  return wrap;
}

function isLast(m) {
  const msgs = state.current.messages;
  return msgs.length && msgs[msgs.length - 1].id === m.id;
}

function updateStreamingUi() {
  const busy = state.current && state.streaming.has(state.current.id);
  $('stop').classList.toggle('hidden', !busy);
  $('send').disabled = !!busy || !state.current;
}

listen('korero-stream', (ev) => {
  const p = ev.payload;
  if (p.done) state.streaming.delete(p.korero);
  if (!state.current || state.current.id !== p.korero) {
    if (p.done) refreshList();
    return;
  }
  const m = state.current.messages.find((x) => x.id === p.messageId);
  if (m) {
    m.display = p.display;
    m.text = p.raw;
    if (p.done) {
      m.error = p.error;
      m.notice = p.notice;
    }
  }
  if (p.done) {
    // Reload so stored fields (raw text, served model) are shown.
    attempt(() => openKorero(p.korero));
    return;
  }
  const node = document.querySelector(`.msg[data-id="${CSS.escape(p.messageId)}"] .bubble`);
  if (node) {
    node.replaceChildren(renderMarkdown(p.raw, chipText(state.current.redactions)));
    const box = $('messages');
    if (box.scrollHeight - box.scrollTop - box.clientHeight < 200) box.scrollTop = box.scrollHeight;
  }
});

$('korero-title').addEventListener('change', () => attempt(async () => {
  if (!state.current) return;
  state.current = await call('update_conversation', { id: state.current.id, update: { title: $('korero-title').value } });
  await refreshList();
}));

for (const b of document.querySelectorAll('.class-toggle button')) {
  b.addEventListener('click', () => attempt(async () => {
    if (!state.current) return;
    state.current = await call('update_conversation', { id: state.current.id, update: { classification: b.dataset.cls } });
    renderKorero();
    await refreshList();
    toast(b.dataset.cls === 'tapu'
      ? 'Marked tapu: this kōrero will only go to models on this device or sovereign infrastructure.'
      : 'Marked noa: any model, with redaction before it leaves the device.');
  }));
}

// ------------------------------------------------------------ provider / model

function fillProviderSelect() {
  const s = state.settings;
  const sel = $('provider-select');
  sel.replaceChildren(...s.providers.map((p) => el('option', { value: p.id, text: `${p.name}${p.effectiveTrust === 'device' ? '' : p.hasKey || p.kind !== 'anthropic' ? '' : ' (no key)'}` })));
  const pick = s.providers.find((p) => p.id === s.lastProvider) || s.providers.find((p) => p.effectiveTrust === 'device') || s.providers[0];
  if (pick) sel.value = pick.id;
  fillModelSelect(s.lastModel);
}

function currentProvider() {
  return state.settings?.providers.find((p) => p.id === $('provider-select').value);
}

function fillModelSelect(prefer) {
  const p = currentProvider();
  const sel = $('model-select');
  const models = p ? p.models : [];
  sel.replaceChildren(...models.map((m) => el('option', { value: m, text: m })));
  if (!models.length) sel.append(el('option', { value: '', text: 'No models: add or fetch them in Settings' }));
  if (prefer && models.includes(prefer)) sel.value = prefer;
  updateRoute();
}

$('provider-select').addEventListener('change', () => {
  fillModelSelect();
  attempt(() => call('set_last_selection', { providerId: $('provider-select').value, model: $('model-select').value }));
});
$('model-select').addEventListener('change', () => {
  updateRoute();
  attempt(() => call('set_last_selection', { providerId: $('provider-select').value, model: $('model-select').value }));
});

function modelActive() {
  return !!(state.model && state.model.installed && state.settings?.kaupapa.useModel);
}

function detectorText() {
  return modelActive() ? 'rules, your lists and the name model' : 'rules and your lists only; the name model is not on';
}

function updateRoute() {
  const p = currentProvider();
  const badge = $('destination');
  const note = $('route-note');
  if (!p) {
    badge.textContent = '';
    note.textContent = '';
    return;
  }
  const trust = p.effectiveTrust;
  const tapu = state.current && state.current.classification === 'tapu';
  const blocked = tapu && trust === 'offshore';
  badge.className = `badge ${blocked ? 'blocked' : trust}`;
  badge.textContent = blocked ? `Tapu: cannot send to ${p.name}` : trust === 'device' ? TRUST_TEXT.device : `${TRUST_TEXT[trust]} · ${p.jurisdiction}`;
  const redact = trust !== 'device' || state.settings.kaupapa.redactOnDevice;
  if (blocked) note.textContent = 'This kōrero is tapu. Choose a model on this device or one you have marked sovereign.';
  else if (trust === 'device') note.textContent = redact ? 'Runs on this device; Tūmataiti still redacts (your setting).' : 'Runs on this device. Nothing leaves it.';
  else note.textContent = `Tūmataiti redacts on this device (${detectorText()}) before sending to ${p.name} (${p.jurisdiction}).`;
}

// ------------------------------------------------------------ sending

$('input').addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
    e.preventDefault();
    $('composer').requestSubmit();
  }
});

$('composer').addEventListener('submit', (e) => {
  e.preventDefault();
  attempt(submit);
});

$('stop').addEventListener('click', () => attempt(() => call('cancel_reply', { id: state.current.id })));

async function submit() {
  const text = $('input').value;
  if (!text.trim()) return;
  if (!state.current) {
    const c = await call('create_conversation', { classification: 'noa' });
    state.current = c;
  }
  const providerId = $('provider-select').value;
  const model = $('model-select').value;
  if (!model) throw new Error('Choose a model first (Settings → Providers can fetch the list).');
  $('send').disabled = true;
  $('route-note').textContent = 'Checking with Tūmataiti…';
  let preview;
  try {
    preview = await call('preview_message', { id: state.current.id, text, providerId, model });
  } finally {
    updateRoute();
    updateStreamingUi();
  }
  let reviewed = false;
  if (preview.blocked || preview.needsReview) {
    const go = await review(text, providerId, model, preview);
    if (!go) return;
    reviewed = true;
  }
  const view = await call('send_message', { id: state.current.id, text, providerId, model, reviewed });
  $('input').value = '';
  state.streaming.add(view.id);
  state.current = view;
  renderKorero();
  await refreshList();
}

const reviewCat = $('review-term-cat');
reviewCat.replaceChildren(...[['PERSON', 'Person'], ['AFFILIATION', 'Affiliation'], ['ADDRESS', 'Place / address'], ['HEALTH', 'Health'], ['OTHER_ID', 'Other ID']]
  .map(([v, t]) => el('option', { value: v, text: t })));

function review(text, providerId, model, preview) {
  return new Promise((resolve) => {
    const dlg = $('review');
    const render = (pv) => {
      const d = pv.destination;
      $('review-title').textContent = pv.blocked ? 'This message cannot be sent' : pv.leavesDevice ? 'Check what will leave your device' : 'Check what will be sent';
      $('review-dest').textContent = `To ${d.name} · ${d.model} · ${TRUST_TEXT[d.trust]} · processed in ${d.jurisdiction}`;
      const blk = $('review-blocked');
      blk.classList.toggle('hidden', !pv.blocked);
      blk.textContent = pv.blocked ? `Blocked because ${pv.blockReasons.join('; ')}.` : '';
      $('review-outbound').replaceChildren(highlightPlaceholders(pv.blocked ? '(nothing will be sent)' : pv.outbound));
      $('review-history').textContent = pv.historyFindings
        ? `Earlier messages in this kōrero are redacted again too (${pv.historyFindings} item${pv.historyFindings === 1 ? '' : 's'}).`
        : '';
      const rows = pv.findings.map((f) => {
        const canAllow = f.category !== 'CULTURAL_RESTRICTED' && f.category !== 'SECRET';
        return el('tr', {},
          el('td', { text: f.text }),
          el('td', { text: catLabel(f.label === 'ID' ? 'OTHER_ID' : f.category) + (f.note ? ` — ${f.note}` : '') }),
          el('td', { text: foundBy(f) }),
          el('td', { text: f.action }),
          el('td', { text: f.replacement ?? '(unchanged)' }),
          el('td', {}, canAllow ? el('button', {
            type: 'button',
            text: 'Allow',
            title: 'Send this unchanged in this kōrero',
            onclick: () => attempt(async () => {
              if (!confirm(`Send “${f.text}” unchanged for the rest of this kōrero?`)) return;
              await call('update_conversation', { id: state.current.id, update: { allow: [...state.current.allow, f.text] } });
              await refresh();
            }),
          }) : null));
      });
      $('review-findings').replaceChildren(...(rows.length ? rows : [el('tr', {}, el('td', { colspan: '6', class: 'muted', text: pv.modelUsed ? 'Tūmataiti found nothing to redact.' : 'Nothing found by the rules or your lists.' }))]));
      $('review-send').disabled = pv.blocked;
      $('review-hint').textContent = pv.modelUsed
        ? 'Checked by the Tūmataiti rules, your kaitiaki lists and the name model. No detector catches everything: read the text above before sending.'
        : 'Checked by the Tūmataiti rules and your kaitiaki lists. Names, places and whakapapa are only caught when they are on your lists or added here (install the name model in Settings → Tūmataiti to find them in context), so read the text above before sending.';
    };
    const focusSafe = (pv) => (pv.blocked ? dlg.querySelector('button[value=cancel]') : $('review-send')).focus();
    const refresh = async () => {
      state.current = await call('get_conversation', { id: state.current.id });
      const pv = await call('preview_message', { id: state.current.id, text, providerId, model });
      render(pv);
      focusSafe(pv);
    };
    $('review-add').onclick = () => attempt(async () => {
      const term = $('review-term').value.trim();
      if (!term) return;
      await call('update_conversation', {
        id: state.current.id,
        update: { terms: [...state.current.terms, { term, category: reviewCat.value }] },
      });
      $('review-term').value = '';
      await refresh();
    });
    dlg.onclose = () => resolve(dlg.returnValue === 'send');
    render(preview);
    dlg.returnValue = '';
    dlg.showModal();
    // Never let Enter land on an "Allow" button: focus Send, or Cancel when blocked.
    focusSafe(preview);
  });
}

// ------------------------------------------------------------ kōrero menu

function closeMenu() {
  document.querySelector('.menu').removeAttribute('open');
}

$('export-md').addEventListener('click', () => attempt(async () => {
  closeMenu();
  if (!state.current) return;
  if (await call('export_conversation', { id: state.current.id, format: 'markdown' })) toast('Exported. The exported file is not encrypted.');
}));
$('export-json').addEventListener('click', () => attempt(async () => {
  closeMenu();
  if (!state.current) return;
  if (await call('export_conversation', { id: state.current.id, format: 'json' })) toast('Exported. The exported file is not encrypted.');
}));
$('delete-korero').addEventListener('click', () => attempt(async () => {
  closeMenu();
  if (!state.current) return;
  if (!confirm(`Delete “${state.current.title}”? This cannot be undone.`)) return;
  await call('delete_conversation', { id: state.current.id });
  state.current = null;
  await refreshList();
  if (state.list.length) await openKorero(state.list[0].id);
  else showEmpty();
}));
$('korero-terms').addEventListener('click', () => {
  closeMenu();
  if (!state.current) return;
  renderTerms();
  $('terms-dialog').showModal();
});

function renderTerms() {
  const c = state.current;
  const save = (update) => attempt(async () => {
    state.current = await call('update_conversation', { id: c.id, update });
    renderTerms();
  });
  $('terms-redact').replaceChildren(...(c.terms.length ? c.terms.map((t, i) => el('li', {},
    el('span', { text: `${t.term} (${catLabel(t.category)})` }),
    el('button', { type: 'button', text: 'Remove', onclick: () => save({ terms: c.terms.filter((_, j) => j !== i) }) }))) : [el('li', { class: 'muted', text: 'None' })]));
  $('terms-allow').replaceChildren(...(c.allow.length ? c.allow.map((a, i) => el('li', {},
    el('span', { text: a }),
    el('button', { type: 'button', text: 'Remove', onclick: () => save({ allow: c.allow.filter((_, j) => j !== i) }) }))) : [el('li', { class: 'muted', text: 'None' })]));
}

// ------------------------------------------------------------ settings

$('open-settings').addEventListener('click', () => attempt(openSettings));
$('close-settings').addEventListener('click', () => $('settings').close());
$('settings').addEventListener('close', () => {
  if (state.settings) {
    fillProviderSelect();
    if (state.current) renderKorero();
  }
});

for (const tab of document.querySelectorAll('.tabs button')) {
  tab.addEventListener('click', () => {
    for (const t of document.querySelectorAll('.tabs button')) t.classList.toggle('active', t === tab);
    for (const p of document.querySelectorAll('.panel')) p.classList.toggle('hidden', p.dataset.panel !== tab.dataset.tab);
    if (tab.dataset.tab === 'audit') attempt(loadAudit);
  });
}

async function openSettings() {
  state.settings = await call('get_settings');
  state.model = await call('model_status');
  renderSettings();
  $('settings').showModal();
}

function renderSettings() {
  const s = state.settings;
  renderProviders();
  // Policy
  const actions = s.kaupapa.policy.actions;
  $('policy-rows').replaceChildren(...CATEGORIES.map(([code, label]) => {
    const sel = el('select', { 'data-cat': code, disabled: !!LOCKED_ACTIONS[code] },
      ...ACTIONS.map((a) => el('option', { value: a, text: a })));
    sel.value = LOCKED_ACTIONS[code] || actions[code] || 'flag';
    return el('tr', {}, el('td', { text: label }), el('td', {}, sel));
  }));
  $('policy-threshold').value = s.kaupapa.policy.threshold;
  $('policy-review').value = s.kaupapa.review;
  $('policy-on-device').checked = s.kaupapa.redactOnDevice;
  $('policy-use-model').checked = s.kaupapa.useModel;
  renderModel();
  // Kaitiaki lists
  const byCat = (cat) => s.kaupapa.gazetteer.filter((t) => t.category === cat).map((t) => t.term).join('\n');
  $('kt-person').value = byCat('PERSON');
  $('kt-affiliation').value = byCat('AFFILIATION');
  $('kt-address').value = byCat('ADDRESS');
  $('kt-health').value = byCat('HEALTH');
  $('kt-tapu').value = s.kaupapa.tapuTerms.join('\n');
  $('kt-allow').value = s.kaupapa.allowList.join('\n');
  // Security
  $('sec-mode').textContent = s.keyMode === 'passphrase'
    ? 'Your data key is protected by a passphrase (Argon2id, 64 MiB). You can change it, or move the key to the OS keychain.'
    : 'Your data key is in the OS keychain. Setting a passphrase is stronger: it removes the key from the keychain.';
  $('sec-use-keychain').classList.toggle('hidden', s.keyMode === 'keychain');
  $('sec-autolock').value = s.autoLockMinutes;
  $('sec-retention').value = s.retentionDays;
  $('sec-system').value = s.systemPrompt;
  $('sec-dir').textContent = s.dataDir;
  $('about-detector').textContent = s.detector;
}

function renderProviders() {
  const cards = state.settings.providers.map(providerCard);
  $('provider-cards').replaceChildren(...cards);
}

function providerCard(p) {
  const isNew = !p.id;
  const f = {
    name: el('input', { value: p.name }),
    kind: el('select', {}, el('option', { value: 'openai_compatible', text: 'OpenAI-compatible' }), el('option', { value: 'anthropic', text: 'Anthropic' })),
    baseUrl: el('input', { value: p.baseUrl, placeholder: 'https://… or http://localhost:…' }),
    jurisdiction: el('input', { value: p.jurisdiction, placeholder: 'Where data is processed' }),
    trust: el('select', { disabled: p.effectiveTrust === 'device' },
      el('option', { value: 'offshore', text: 'Third-party cloud' }),
      el('option', { value: 'sovereign', text: 'Sovereign: infrastructure we control' }),
      p.effectiveTrust === 'device' ? el('option', { value: 'device', text: 'On this device (detected from the address)' }) : null),
    models: el('textarea', { rows: '3' }),
    effort: el('select', {}, ...['', 'low', 'medium', 'high', 'xhigh', 'max'].map((e) => el('option', { value: e, text: e || '(default)' }))),
    maxTokens: el('input', { type: 'number', min: '1', max: '128000', value: String(p.maxTokens) }),
    key: el('input', { type: 'password', placeholder: p.hasKey ? 'A key is stored (hidden). Type to replace it.' : 'API key', autocomplete: 'off' }),
  };
  f.kind.value = p.kind;
  f.trust.value = p.effectiveTrust === 'device' ? 'device' : p.trust === 'sovereign' ? 'sovereign' : 'offshore';
  f.models.value = p.models.join('\n');
  f.effort.value = p.effort || '';
  const trustBadge = el('span', { class: `badge ${p.effectiveTrust}`, text: TRUST_TEXT[p.effectiveTrust] });
  const collect = () => ({
    id: p.id || '',
    name: f.name.value,
    kind: f.kind.value,
    baseUrl: f.baseUrl.value,
    models: f.models.value.split(/[\n,]/).map((m) => m.trim()).filter(Boolean),
    jurisdiction: f.jurisdiction.value,
    trust: f.trust.value,
    effort: f.effort.value || null,
    maxTokens: Number(f.maxTokens.value) || 4096,
  });
  const save = async () => {
    state.settings = await call('save_provider', { provider: collect() });
    renderProviders();
    toast('Provider saved.');
  };
  return el('div', { class: 'provider-card' },
    el('h4', {}, el('span', { text: isNew ? 'New provider' : p.name }), isNew ? null : trustBadge, p.hasKey ? el('span', { class: 'small muted', text: '· key stored' }) : null),
    el('div', { class: 'grid2' },
      el('label', {}, 'Name', f.name),
      el('label', {}, 'API type', f.kind),
      el('label', {}, 'Base URL', f.baseUrl),
      el('label', {}, 'Where it processes data', f.jurisdiction),
      el('label', {}, 'Trust (on-device is detected from the address)', f.trust),
      el('label', {}, 'Max tokens per reply', f.maxTokens),
      el('label', {}, 'Models (one per line)', f.models),
      el('label', {}, 'Effort (Anthropic only)', f.effort)),
    el('div', { class: 'row' },
      el('button', { class: 'primary', text: 'Save', onclick: () => attempt(save) }),
      isNew ? null : el('button', { text: 'Fetch models', onclick: () => attempt(async () => {
        state.settings = await call('fetch_models', { providerId: p.id });
        renderProviders();
        toast('Model list updated.');
      }) }),
      isNew ? null : el('button', { class: 'danger', text: 'Delete', onclick: () => attempt(async () => {
        if (!confirm(`Delete ${p.name} and its stored key?`)) return;
        state.settings = await call('delete_provider', { providerId: p.id });
        renderProviders();
      }) })),
    isNew ? el('p', { class: 'hint', text: 'Save first, then add an API key.' }) : el('div', { class: 'row' },
      f.key,
      el('button', { text: 'Save key', onclick: () => attempt(async () => {
        state.settings = await call('set_api_key', { providerId: p.id, key: f.key.value });
        f.key.value = '';
        renderProviders();
        toast('Key stored, encrypted. It is never shown again.');
      }) }),
      p.hasKey ? el('button', { text: 'Remove key', onclick: () => attempt(async () => {
        state.settings = await call('set_api_key', { providerId: p.id, key: null });
        renderProviders();
      }) }) : null));
}

function mb(bytes) {
  return `${Math.round(bytes / 1e6)} MB`;
}

function renderModel() {
  const m = state.model;
  if (!m) return;
  const status = $('model-status');
  status.className = m.installed ? 'ok' : '';
  if (!m.supported) {
    status.textContent = 'ONNX Runtime has no build for this computer (for example Intel Macs), so the name model cannot run here. The rules and kaitiaki lists still work.';
  } else if (m.installed) {
    status.textContent = `Installed: ${m.name} (${m.precision}), ${m.runtime}. ${m.enabled ? 'In use.' : 'Switched off below.'}`;
  } else {
    status.textContent = `Not installed. Installing downloads about ${mb(m.downloadBytes)} once, from ${m.sources.join(' and ')}. Each file is checked against a SHA-256 built into Pātaka before it is used. No text is sent.`;
  }
  $('model-install').classList.toggle('hidden', !m.supported || m.installed);
  $('model-install').disabled = m.installing;
  $('model-remove').classList.toggle('hidden', !m.installed);
}

$('model-install').addEventListener('click', () => attempt(async () => {
  $('model-install').disabled = true;
  $('model-progress').textContent = 'Starting…';
  try {
    state.model = await call('model_install');
    $('model-progress').textContent = 'Installed and checked.';
    toast('The Tūmataiti name model is installed.');
  } catch (e) {
    $('model-progress').textContent = '';
    throw e;
  } finally {
    state.model = await call('model_status');
    renderModel();
    updateRoute();
  }
}));

$('model-remove').addEventListener('click', () => attempt(async () => {
  if (!confirm('Remove the name model? Names will then only be caught by your kaitiaki lists.')) return;
  state.model = await call('model_remove');
  renderModel();
  updateRoute();
}));

listen('model-progress', (ev) => {
  const p = ev.payload;
  $('model-progress').textContent = `Downloading ${p.stage}: ${mb(p.done)} of ${mb(p.total)}`;
});

$('add-provider').addEventListener('click', () => {
  $('provider-cards').append(providerCard({
    id: '', name: '', kind: 'openai_compatible', baseUrl: '', models: [], jurisdiction: '', trust: 'offshore',
    effort: null, maxTokens: 8192, hasKey: false, effectiveTrust: 'offshore',
  }));
});

$('save-policy').addEventListener('click', () => attempt(async () => {
  const k = structuredClone(state.settings.kaupapa);
  for (const sel of document.querySelectorAll('#policy-rows select')) k.policy.actions[sel.dataset.cat] = sel.value;
  k.policy.threshold = Number($('policy-threshold').value);
  k.review = $('policy-review').value;
  k.redactOnDevice = $('policy-on-device').checked;
  k.useModel = $('policy-use-model').checked;
  state.settings = await call('save_kaupapa', { kaupapa: k });
  toast('Tūmataiti policy saved.');
}));

$('save-kaitiaki').addEventListener('click', () => attempt(async () => {
  const lines = (id) => $(id).value.split('\n').map((s) => s.trim()).filter(Boolean);
  const k = structuredClone(state.settings.kaupapa);
  k.gazetteer = [
    ...lines('kt-person').map((term) => ({ term, category: 'PERSON' })),
    ...lines('kt-affiliation').map((term) => ({ term, category: 'AFFILIATION' })),
    ...lines('kt-address').map((term) => ({ term, category: 'ADDRESS' })),
    ...lines('kt-health').map((term) => ({ term, category: 'HEALTH' })),
  ];
  k.tapuTerms = lines('kt-tapu');
  k.allowList = lines('kt-allow');
  state.settings = await call('save_kaupapa', { kaupapa: k });
  toast('Kaitiaki lists saved.');
}));

$('save-general').addEventListener('click', () => attempt(async () => {
  state.settings = await call('save_general', {
    input: {
      retentionDays: Number($('sec-retention').value) || 0,
      autoLockMinutes: Number($('sec-autolock').value) || 0,
      systemPrompt: $('sec-system').value,
    },
  });
  toast('Saved.');
}));

$('sec-use-pass').addEventListener('click', () => attempt(async () => {
  const p1 = $('sec-pass1').value;
  if (p1 !== $('sec-pass2').value) throw new Error('The passphrases do not match.');
  await call('vault_change', { mode: 'passphrase', passphrase: p1 });
  $('sec-pass1').value = '';
  $('sec-pass2').value = '';
  state.settings = await call('get_settings');
  renderSettings();
  toast('Passphrase set.');
}));

$('sec-use-keychain').addEventListener('click', () => attempt(async () => {
  if (!confirm('Move the data key to the OS keychain? Pātaka will then unlock without a passphrase while you are signed in.')) return;
  await call('vault_change', { mode: 'keychain', passphrase: null });
  state.settings = await call('get_settings');
  renderSettings();
  toast('Key moved to the OS keychain.');
}));

$('wipe').addEventListener('click', () => attempt(async () => {
  await call('wipe_everything', { confirm: $('wipe-confirm').value });
  $('wipe-confirm').value = '';
  state.current = null;
  state.settings = null;
  $('settings').close();
  await showLock();
  toast('Everything has been deleted.');
}));

async function loadAudit() {
  const rows = await call('audit_entries', { limit: 300 });
  $('audit-rows').replaceChildren(...rows.map((e) => {
    const found = Object.entries(e.categories || {}).map(([k, v]) => `${v} ${k}`).join(', ');
    const dest = [e.provider, e.model, e.jurisdiction, e.trust].filter(Boolean).join(' · ');
    return el('tr', {},
      el('td', { text: fmtTime(e.ts) }),
      el('td', { text: e.event + (e.detail ? ` (${e.detail})` : '') }),
      el('td', { text: dest }),
      el('td', { text: e.korero ? e.korero.slice(0, 8) : '' }),
      el('td', { text: found }),
      el('td', { text: e.policyHash || '' }));
  }));
}

$('audit-verify').addEventListener('click', () => attempt(async () => {
  const v = await call('audit_verify');
  $('audit-status').textContent = v.ok
    ? `Chain intact: ${v.entries} entries.`
    : `Chain broken at entry ${v.firstBad} of ${v.entries}. The log has been changed outside Pātaka.`;
}));

// ------------------------------------------------------------ auto-lock

for (const ev of ['mousemove', 'keydown', 'mousedown', 'wheel', 'touchstart']) {
  window.addEventListener(ev, () => { state.lastActivity = Date.now(); }, { passive: true });
}
setInterval(() => {
  const s = state.settings;
  if (!s || !s.autoLockMinutes || state.streaming.size) return;
  if (Date.now() - state.lastActivity > s.autoLockMinutes * 60_000) attempt(lockNow);
}, 15_000);

// ------------------------------------------------------------ start

syncSetupMode();
(async () => {
  const status = await invoke('vault_status');
  if (status.unlocked) await enterApp();
  else await showLock();
})();
