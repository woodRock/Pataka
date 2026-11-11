// A small Markdown renderer that builds DOM nodes with textContent only.
// Model output is never parsed as HTML. Links are shown as text with their
// address, not made clickable, and images are never loaded, so a reply
// cannot make the app contact another server (for example by putting a
// restored value into a URL).
//
// `put(parent, text)` appends a run of plain text. The default appends it as
// is; the chat passes one that turns placeholders into chips. It must only
// create nodes with textContent, like everything else here.

const plain = (parent, text) => parent.append(text);

function inline(parent, text, put) {
  // Underscore emphasis is not supported, so placeholders such as [NZ_GOV_ID] render as typed.
  const re = /(`[^`\n]+`)|(\*\*[^*\n]+\*\*)|(\*[^*\s][^*\n]*\*)|(!?\[[^\]\n]*\]\([^)\s]*\))/g;
  let last = 0;
  let m;
  while ((m = re.exec(text))) {
    if (m.index > last) put(parent, text.slice(last, m.index));
    const tok = m[0];
    let el;
    if (m[1]) {
      el = document.createElement('code');
      put(el, tok.slice(1, -1));
    } else if (m[2]) {
      el = document.createElement('strong');
      inline(el, tok.slice(2, -2), put);
    } else if (m[3]) {
      el = document.createElement('em');
      inline(el, tok.slice(1, -1), put);
    } else {
      const lm = /^(!?)\[([^\]]*)\]\(([^)\s]*)\)$/.exec(tok);
      el = document.createElement('span');
      put(el, lm[1] ? `[image: ${lm[2]}]` : lm[2]);
      const u = document.createElement('span');
      u.className = 'link-url';
      u.textContent = ` (${lm[3]})`;
      el.append(u);
    }
    parent.append(el);
    last = m.index + tok.length;
  }
  if (last < text.length) put(parent, text.slice(last));
}

function inlineLines(parent, lines, put) {
  lines.forEach((l, i) => {
    if (i) parent.append(document.createElement('br'));
    inline(parent, l, put);
  });
}

export function renderMarkdown(src, put = plain) {
  const root = document.createElement('div');
  root.className = 'md';
  const lines = src.replace(/\r/g, '').split('\n');
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    const fence = /^\s*(```|~~~)(.*)$/.exec(line);
    if (fence) {
      const body = [];
      i++;
      while (i < lines.length && !lines[i].trim().startsWith(fence[1])) body.push(lines[i++]);
      i++;
      const pre = document.createElement('pre');
      const code = document.createElement('code');
      put(code, body.join('\n'));
      pre.append(code);
      root.append(pre);
      continue;
    }
    const h = /^(#{1,6})\s+(.*)$/.exec(line);
    if (h) {
      const el = document.createElement(`h${Math.min(h[1].length + 1, 6)}`);
      inline(el, h[2], put);
      root.append(el);
      i++;
      continue;
    }
    if (/^\s*([-*+]|\d+[.)])\s+/.test(line)) {
      const ordered = /^\s*\d+[.)]\s+/.test(line);
      const list = document.createElement(ordered ? 'ol' : 'ul');
      while (i < lines.length && /^\s*([-*+]|\d+[.)])\s+/.test(lines[i])) {
        const li = document.createElement('li');
        inline(li, lines[i].replace(/^\s*([-*+]|\d+[.)])\s+/, ''), put);
        list.append(li);
        i++;
      }
      root.append(list);
      continue;
    }
    if (/^\s*>/.test(line)) {
      const quote = [];
      while (i < lines.length && /^\s*>/.test(lines[i])) quote.push(lines[i++].replace(/^\s*>\s?/, ''));
      const bq = document.createElement('blockquote');
      inlineLines(bq, quote, put);
      root.append(bq);
      continue;
    }
    if (/^\s*(---|\*\*\*|___)\s*$/.test(line)) {
      root.append(document.createElement('hr'));
      i++;
      continue;
    }
    if (!line.trim()) {
      i++;
      continue;
    }
    const para = [];
    while (
      i < lines.length && lines[i].trim() &&
      !/^\s*(```|~~~|#{1,6}\s|>|([-*+]|\d+[.)])\s+)/.test(lines[i])
    ) para.push(lines[i++]);
    const p = document.createElement('p');
    inlineLines(p, para, put);
    root.append(p);
  }
  return root;
}
