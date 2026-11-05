//! Driving a page the way a person would: read what is on it, click, type,
//! press keys, scroll, go back, and wait for something to show up. Each
//! action is a script run through `browser.eval` (as a plain expression, so
//! a strict Content Security Policy does not stop it); the CLI's
//! `deck browser …` verbs and the MCP tools share this.
//!
//! Elements are named the way a snapshot lists them: `[N]` refs, which the
//! snapshot tags with `data-deck-ref`, `text=Some text`, or a CSS selector.
//! What the caller says only ever reaches the page as JSON.

use crate::control;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Helpers every action script starts with.
const PRELUDE: &str = r#"
const D = document, W = window;
const clean = (s, n) => {
  s = String(s == null ? '' : s).replace(/\s+/g, ' ').trim();
  return n && s.length > n ? s.slice(0, n - 1) + '…' : s;
};
// Every element in document order, open shadow roots included.
const each = (root, fn) => {
  const w = D.createTreeWalker(root, 1);
  for (let n = w.nextNode(); n; n = w.nextNode()) {
    if (fn(n) === false) return false;
    if (n.shadowRoot && each(n.shadowRoot, fn) === false) return false;
  }
};
const deepQuery = (sel, all) => {
  const out = [];
  for (const el of D.querySelectorAll(sel)) { out.push(el); if (!all) return out; }
  each(D, (n) => {
    if (!n.shadowRoot) return;
    for (const el of n.shadowRoot.querySelectorAll(sel)) { out.push(el); if (!all) return false; }
  });
  return out;
};
const visible = (el, depth) => {
  if (!el || !el.isConnected || el.nodeType !== 1) return false;
  try { if (el.checkVisibility && !el.checkVisibility({ visibilityProperty: true, checkVisibilityCSS: true })) return false; } catch (_) {}
  const r = el.getBoundingClientRect();
  if (r.width === 0 || r.height === 0) {
    if ((depth || 0) > 2) return false;
    for (const c of el.children) if (visible(c, (depth || 0) + 1)) return true;
    return false;
  }
  const s = getComputedStyle(el);
  return s.visibility !== 'hidden' && s.visibility !== 'collapse';
};
const SEL = 'a[href],button,input:not([type=hidden]),textarea,select,summary,' +
  '[role=button],[role=link],[role=tab],[role=menuitem],[role=menuitemcheckbox],[role=menuitemradio],' +
  '[role=checkbox],[role=radio],[role=switch],[role=option],[role=combobox],[role=textbox],' +
  '[role=searchbox],[role=slider],[role=treeitem],[contenteditable]:not([contenteditable=false]),[onclick],[tabindex]';
// What an element says. WebKit can leave `innerText` empty for laid-out,
// visible text while the page is in a background tab; then its text nodes.
const textOf = (el) => {
  const t = el.innerText;
  if (t && t.trim()) return t;
  let s = '';
  const w = D.createTreeWalker(el, 4);
  for (let n = w.nextNode(); n && s.length < 2000; n = w.nextNode()) {
    const p = n.parentElement;
    if (p && !/^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE)$/.test(p.tagName)) s += n.data + ' ';
  }
  return s;
};
const NATIVE = /^(A|BUTTON|INPUT|TEXTAREA|SELECT|SUMMARY)$/;
const ROLES = /^(button|link|tab|menuitem|menuitemcheckbox|menuitemradio|checkbox|radio|switch|option|combobox|textbox|searchbox|slider|treeitem)$/;
const EDIT = 'input:not([type=hidden]),textarea,select,[contenteditable]:not([contenteditable=false]),[role=textbox],[role=searchbox],[role=combobox]';
const strong = (el) => NATIVE.test(el.tagName) || ROLES.test((el.getAttribute('role') || '').trim().split(/\s+/)[0])
  || (el.isContentEditable && el.hasAttribute('contenteditable'));
// Clickable or typable. A bare `[onclick]` or `[tabindex]` counts when it
// is in the tab order (or has the handler) and not inside another control,
// which is the one to use.
const interactive = (el) => {
  if (el === D.body || el === D.documentElement || !el.matches(SEL) || el.closest('[inert]')) return false;
  if (strong(el)) return true;
  if (!el.hasAttribute('onclick') && el.tabIndex < 0) return false;
  for (let p = el.parentElement, i = 0; p && p !== D.body && i < 12; p = p.parentElement, i++) {
    if (strong(p) || p.hasAttribute('onclick')) return false;
  }
  return true;
};
const editable = (el) => el.matches(EDIT) || el.isContentEditable;
const disabled = (el) => {
  try { if (el.matches(':disabled')) return true; } catch (_) {}
  return el.getAttribute('aria-disabled') === 'true';
};
const TYPES = { button: 'button', submit: 'button', reset: 'button', image: 'button', file: 'button',
  checkbox: 'checkbox', radio: 'radio', range: 'slider', search: 'searchbox', number: 'spinbutton' };
const role = (el) => {
  const r = el.getAttribute('role');
  if (r && r.trim()) return r.trim().split(/\s+/)[0];
  const tag = el.tagName.toLowerCase();
  if (tag === 'a') return 'link';
  if (tag === 'button' || tag === 'summary') return 'button';
  if (tag === 'select') return el.multiple || el.size > 1 ? 'listbox' : 'combobox';
  if (tag === 'textarea') return 'textbox';
  if (tag === 'input') return TYPES[(el.type || 'text').toLowerCase()] || 'textbox';
  if (el.isContentEditable) return 'textbox';
  return tag;
};
const nameOf = (el) => {
  const a = el.getAttribute('aria-label');
  if (a && a.trim()) return clean(a, 80);
  const by = el.getAttribute('aria-labelledby');
  if (by) {
    const root = el.getRootNode();
    const t = by.split(/\s+/).map((id) => {
      const n = (root.getElementById ? root : D).getElementById(id) || D.getElementById(id);
      return n ? textOf(n) : '';
    }).join(' ');
    if (t.trim()) return clean(t, 80);
  }
  if (el.labels && el.labels.length) {
    const t = Array.from(el.labels).map(textOf).join(' ');
    if (t.trim()) return clean(t, 80);
  }
  const tag = el.tagName;
  const field = tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT';
  if (!field) {
    const t = textOf(el);
    if (t.trim()) return clean(t, 80);
  }
  if (tag === 'INPUT' && /^(submit|button|reset)$/i.test(el.type) && el.value) return clean(el.value, 80);
  for (const k of ['placeholder', 'title', 'alt']) {
    const v = el.getAttribute(k);
    if (v && v.trim()) return clean(v, 80);
  }
  if (!field) {
    const inner = el.querySelector('img[alt],[aria-label],svg title');
    const v = inner && (inner.getAttribute('alt') || inner.getAttribute('aria-label') || inner.textContent);
    if (v && v.trim()) return clean(v, 80);
  }
  const n = el.getAttribute('name');
  if (n) return clean(n, 80);
  if (tag === 'INPUT' && el.type === 'submit') return 'Submit';
  return '';
};
const ref = (el) => {
  const refs = W.__deckRefs || [];
  const i = refs.indexOf(el);
  return i > 0 ? '[' + i + '] ' : '';
};
const desc = (el) => {
  const n = nameOf(el);
  return ref(el) + role(el) + (n ? ' "' + n + '"' : '');
};
// The suffixes a snapshot line carries: where a link goes, a field's
// value (never a password's), and states.
const state = (el) => {
  const bits = [];
  const tag = el.tagName;
  if (tag === 'A' && el.href && !/^javascript:/i.test(el.href)) {
    let h = el.href;
    try { const u = new URL(el.href); if (u.origin === location.origin) h = u.pathname + u.search + u.hash; } catch (_) {}
    bits.push('→ ' + clean(h, 100));
  }
  const type = tag === 'INPUT' ? (el.type || 'text').toLowerCase() : '';
  if (type === 'password') {
    bits.push(el.value ? '(password, filled)' : '(password)');
  } else if (tag === 'SELECT') {
    const o = el.options[el.selectedIndex];
    if (o) bits.push('value="' + clean(o.text, 60) + '"');
  } else if ((tag === 'TEXTAREA' || (tag === 'INPUT' && !/^(checkbox|radio|submit|button|reset|image|file|range|color)$/.test(type))) && el.value) {
    bits.push('value="' + clean(el.value, 60) + '"');
  } else if (el.isContentEditable && tag !== 'INPUT' && tag !== 'TEXTAREA') {
    const t = clean(textOf(el), 60);
    if (t) bits.push('value="' + t + '"');
  }
  if (type && !/^(text|password|submit|button|reset|checkbox|radio|search|image)$/.test(type)) bits.push('type=' + type);
  if (el.checked === true || el.getAttribute('aria-checked') === 'true') bits.push('checked');
  if (el.getAttribute('aria-checked') === 'mixed' || el.indeterminate === true) bits.push('mixed');
  if (el.getAttribute('aria-pressed') === 'true') bits.push('pressed');
  if (el.getAttribute('aria-selected') === 'true') bits.push('selected');
  const ex = el.getAttribute('aria-expanded');
  if (ex === 'true') bits.push('expanded'); else if (ex === 'false') bits.push('collapsed');
  if (tag === 'SUMMARY' && el.parentElement && el.parentElement.tagName === 'DETAILS') bits.push(el.parentElement.open ? 'expanded' : 'collapsed');
  if (disabled(el)) bits.push('disabled');
  if (el.readOnly === true) bits.push('readonly');
  return bits.join(' ');
};
const candidates = (test, cap) => {
  const out = [];
  each(D, (el) => {
    if (out.length >= cap) return false;
    if (test(el) && visible(el)) out.push(el);
  });
  return out;
};
const words = (s) => clean(s).toLowerCase().split(' ').filter((w) => w.length > 2);
// Any visible text that says `w`: the exact text first, then the
// shortest that has it.
const byPlainText = (w) => {
  const tw = D.createTreeWalker(D.body || D.documentElement, 4);
  let part = null, partLen = Infinity;
  for (let n = tw.nextNode(); n; n = tw.nextNode()) {
    const t = clean(n.data).toLowerCase();
    if (!t || !t.includes(w)) continue;
    const el = n.parentElement;
    if (!el || /^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE)$/.test(el.tagName) || !visible(el)) continue;
    if (t === w) return el;
    if (t.length < partLen) { part = el; partLen = t.length; }
  }
  return part;
};
// The control `want` names: exact text first, then the shortest text
// that contains it. Falls back to any visible text on the page, or with
// `plainFirst` (scrolling to something) starts there.
const byText = (want, test, plainFirst) => {
  const w = clean(want).toLowerCase();
  if (!w) return { error: 'text= needs some text' };
  if (plainFirst) { const el = byPlainText(w); if (el) return { el }; }
  const pool = candidates(test, 5000);
  let best = null, bestLen = Infinity;
  for (const el of pool) {
    for (const t0 of [nameOf(el), clean(textOf(el) || el.value || '')]) {
      const t = t0.toLowerCase();
      if (!t) continue;
      if (t === w) return { el };
      if (t.includes(w) && t.length < bestLen) { best = el; bestLen = t.length; }
    }
  }
  if (best) return { el: best };
  // Plain text, e.g. a div with a click handler deck cannot see.
  const plain = plainFirst ? null : byPlainText(w);
  if (plain) return { el: plain };
  const near = [];
  const ws = words(want);
  for (const el of pool) {
    const n = nameOf(el);
    if (n && ws.some((x) => n.toLowerCase().includes(x))) near.push('"' + n + '"');
    if (near.length >= 6) break;
  }
  const some = near.length ? near : pool.slice(0, 8).map((el) => '"' + nameOf(el) + '"').filter((s) => s !== '""');
  return { error: 'nothing on the page says "' + clean(want) + '"' + (some.length ? '; there is ' + some.join(', ') : '') };
};
// A snapshot ref, `text=…`, or a CSS selector.
const find = (target, wantEdit, plainFirst) => {
  const t = String(target == null ? '' : target).trim();
  if (!t) return { error: 'no target: give a [N] ref from snapshot, text=Some text, or a CSS selector' };
  const m = /^\[?(\d+)\]?$/.exec(t);
  if (m) {
    const n = +m[1];
    let el = (W.__deckRefs || [])[n];
    if (!el || !el.isConnected) el = deepQuery('[data-deck-ref="' + n + '"]', false)[0];
    if (!el) return { error: 'no element [' + n + '] on this page; take a new snapshot (refs change with the page)' };
    return { el };
  }
  const tm = /^text\s*=\s*([\s\S]*)$/i.exec(t);
  if (tm) {
    const want = tm[1].trim().replace(/^(["'])([\s\S]*)\1$/, '$2');
    return byText(want, wantEdit ? editable : interactive, plainFirst);
  }
  let list;
  try { list = deepQuery(t, true); } catch (e) {
    return { error: 'not a [N] ref, text=… or a valid CSS selector: ' + t };
  }
  if (!list.length) return { error: 'nothing matches the selector ' + t };
  return { el: list.find((el) => visible(el)) || list[0] };
};
const scrollTo = (el) => {
  try { el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' }); }
  catch (_) { try { el.scrollIntoView({ block: 'center', inline: 'center' }); } catch (_) { el.scrollIntoView(); } }
};
// Before an action: mark this document, notice a navigation starting, and
// answer dialogs it raises (the recorder reads `__deckAct`).
const arm = (a) => {
  W.__deckTok = a.tok;
  W.__deckLeaving = 0;
  try { if (W.__deckLeave) W.removeEventListener('beforeunload', W.__deckLeave); } catch (_) {}
  W.__deckLeave = () => { W.__deckLeaving = 1; };
  W.addEventListener('beforeunload', W.__deckLeave);
  W.__deckAct = { tok: a.tok, from: location.href, until: Date.now() + 3000, accept: !a.dismiss, prompt: a.prompt, dialogs: [] };
};
const done = (r) => {
  const act = W.__deckAct;
  if (act && act.dialogs && act.dialogs.length) r.dialogs = act.dialogs;
  return r;
};
const deepActive = () => {
  let a = D.activeElement;
  while (a && a.shadowRoot && a.shadowRoot.activeElement) a = a.shadowRoot.activeElement;
  return a && a !== D.body && a !== D.documentElement ? a : null;
};
const KEYS = {
  enter: ['Enter', 'Enter', 13], return: ['Enter', 'Enter', 13], escape: ['Escape', 'Escape', 27],
  esc: ['Escape', 'Escape', 27], tab: ['Tab', 'Tab', 9], backspace: ['Backspace', 'Backspace', 8],
  delete: ['Delete', 'Delete', 46], del: ['Delete', 'Delete', 46], space: [' ', 'Space', 32],
  arrowup: ['ArrowUp', 'ArrowUp', 38], up: ['ArrowUp', 'ArrowUp', 38],
  arrowdown: ['ArrowDown', 'ArrowDown', 40], down: ['ArrowDown', 'ArrowDown', 40],
  arrowleft: ['ArrowLeft', 'ArrowLeft', 37], left: ['ArrowLeft', 'ArrowLeft', 37],
  arrowright: ['ArrowRight', 'ArrowRight', 39], right: ['ArrowRight', 'ArrowRight', 39],
  home: ['Home', 'Home', 36], end: ['End', 'End', 35], pageup: ['PageUp', 'PageUp', 33],
  pagedown: ['PageDown', 'PageDown', 34],
};
const kev = (type, k, mods) => {
  const init = Object.assign({ key: k[0], code: k[1], keyCode: k[2], which: k[2], bubbles: true, cancelable: true, composed: true, view: W }, mods);
  if (type === 'keypress') init.charCode = k[0].length === 1 ? k[0].charCodeAt(0) : k[2];
  const ev = new KeyboardEvent(type, init);
  if (ev.keyCode !== k[2]) {
    try {
      Object.defineProperty(ev, 'keyCode', { get: () => k[2] });
      Object.defineProperty(ev, 'which', { get: () => k[2] });
    } catch (_) {}
  }
  return ev;
};
const submit = (form) => {
  if (form.requestSubmit) { try { form.requestSubmit(); return; } catch (_) {} }
  const b = form.querySelector('button[type=submit],input[type=submit],button:not([type])');
  if (b) b.click(); else form.submit();
};
// Enter on `el` as a keyboard would: the page may take it; if not, a
// field submits its form, a button or link activates, a text area breaks
// the line.
const enter = (el, mods) => {
  const k = KEYS.enter;
  const go = el.dispatchEvent(kev('keydown', k, mods || {}));
  let did = '';
  if (go) {
    el.dispatchEvent(kev('keypress', k, mods || {}));
    const tag = el.tagName;
    if (tag === 'INPUT' && el.form && !/^(button|submit|reset|image|checkbox|radio|file)$/i.test(el.type)) { submit(el.form); did = 'submitted'; }
    else if (tag === 'TEXTAREA' || (el.isContentEditable && tag !== 'INPUT')) { try { D.execCommand('insertLineBreak'); } catch (_) {} }
    else if (tag === 'A' || tag === 'BUTTON' || tag === 'SUMMARY' || (tag === 'INPUT' && /^(button|submit|reset|image)$/i.test(el.type)) || el.getAttribute('role') === 'button') { el.click(); did = 'activated'; }
  }
  el.dispatchEvent(kev('keyup', k, mods || {}));
  return did;
};
const setValue = (el, v) => {
  const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype
    : el instanceof HTMLSelectElement ? HTMLSelectElement.prototype : HTMLInputElement.prototype;
  const d = Object.getOwnPropertyDescriptor(proto, 'value');
  if (d && d.set) d.set.call(el, v); else el.value = v;
  const ie = typeof InputEvent === 'function'
    ? new InputEvent('input', { bubbles: true, composed: true, inputType: 'insertText', data: v })
    : new Event('input', { bubbles: true, composed: true });
  el.dispatchEvent(ie);
  el.dispatchEvent(new Event('change', { bubbles: true }));
};
// What a scroll moves: the page, unless it is locked (an app layout, an
// open modal), then the scrollable box under the middle of the view, or
// the largest one.
const scrolls = (el) => el.scrollHeight > el.clientHeight + 1 && /(auto|scroll|overlay)/.test(getComputedStyle(el).overflowY);
const scroller = () => {
  const se = D.scrollingElement || D.documentElement;
  const hs = getComputedStyle(D.documentElement).overflowY, bs = D.body ? getComputedStyle(D.body).overflowY : 'visible';
  const locked = hs === 'hidden' || hs === 'clip' || (hs === 'visible' && (bs === 'hidden' || bs === 'clip'));
  if (!locked && se.scrollHeight > se.clientHeight + 1) return se;
  for (let el = D.elementFromPoint(innerWidth / 2, innerHeight / 2); el && el !== D.body && el !== D.documentElement;
    el = el.parentElement || (el.getRootNode && el.getRootNode().host) || null) {
    if (scrolls(el)) return el;
  }
  let best = null, area = 0;
  each(D, (n) => {
    if (n.clientHeight > 50 && scrolls(n) && visible(n)) {
      const a = n.clientWidth * n.clientHeight;
      if (a > area) { area = a; best = n; }
    }
  });
  return best || se;
};
// The field a target means: itself, its label's control, a field inside
// it, or the one next to the text that names it.
const fieldOf = (el) => {
  if (editable(el)) return el;
  const lab = el.closest('label');
  if (lab && lab.control) return lab.control;
  const inner = el.querySelector && el.querySelector(EDIT);
  if (inner) return inner;
  for (let p = el.parentElement, i = 0; p && p !== D.body && i < 3; p = p.parentElement, i++) {
    const f = Array.from(p.querySelectorAll(EDIT)).find((x) => visible(x));
    if (f) return f;
  }
  return null;
};
"#;

/// The page as an agent reads it: its headings and the controls on it,
/// numbered. Each control is tagged `data-deck-ref=N` for the next action.
const SNAPSHOT: &str = r#"(a) => {
  const MAX = 300;
  for (const el of W.__deckRefs || []) { try { if (el) el.removeAttribute('data-deck-ref'); } catch (_) {} }
  for (const el of deepQuery('[data-deck-ref]', true)) el.removeAttribute('data-deck-ref');
  const all = candidates(interactive, 5000);
  let picked = all;
  if (all.length > MAX) {
    const vh = innerHeight, vw = innerWidth;
    const inView = (el) => { const r = el.getBoundingClientRect(); return r.bottom > 0 && r.top < vh && r.right > 0 && r.left < vw; };
    const keep = new Set(all.filter(inView).slice(0, MAX));
    for (const el of all) { if (keep.size >= MAX) break; keep.add(el); }
    picked = all.filter((el) => keep.has(el));
  }
  const refs = [null];
  const elements = picked.map((el) => {
    refs.push(el);
    const n = refs.length - 1;
    try { el.setAttribute('data-deck-ref', String(n)); } catch (_) {}
    const nm = nameOf(el), st = state(el);
    return '[' + n + '] ' + role(el) + (nm ? ' "' + nm + '"' : '') + (st ? ' ' + st : '');
  });
  try { Object.defineProperty(W, '__deckRefs', { value: refs, configurable: true, writable: true }); } catch (_) { W.__deckRefs = refs; }
  const headings = [];
  for (const h of deepQuery('h1,h2,h3', true)) {
    if (headings.length >= 40) break;
    if (!visible(h)) continue;
    const t = clean(textOf(h), 100);
    if (t) headings.push('#'.repeat(+h.tagName[1]) + ' ' + t);
  }
  const r = { title: D.title, url: location.href, headings, elements, more: all.length - picked.length };
  const f = deepActive();
  if (f) { const i = refs.indexOf(f); if (i > 0) r.focused = i; }
  return r;
}"#;

const CLICK: &str = r#"(a) => {
  const f = find(a.target, false);
  if (f.error) return f;
  const el = f.el;
  if (disabled(el)) return { error: desc(el) + ' is disabled' };
  arm(a);
  scrollTo(el);
  const r = el.getBoundingClientRect();
  const x = r.left + r.width / 2, y = r.top + r.height / 2;
  const res = { clicked: desc(el) };
  try {
    const root = el.getRootNode && el.getRootNode().elementFromPoint ? el.getRootNode() : D;
    const top = root.elementFromPoint(x, y);
    if (top && top !== el && !el.contains(top) && !top.contains(el)) {
      res.note = 'something else is on top of it at its center: ' + desc(top);
    }
  } catch (_) {}
  const base = { bubbles: true, cancelable: true, composed: true, view: W, clientX: x, clientY: y, screenX: x, screenY: y, button: 0 };
  const P = typeof PointerEvent === 'function' ? PointerEvent : MouseEvent;
  const ptr = (extra) => Object.assign({ pointerId: 1, pointerType: 'mouse', isPrimary: true }, base, extra);
  el.dispatchEvent(new P('pointerover', ptr({})));
  el.dispatchEvent(new MouseEvent('mouseover', base));
  el.dispatchEvent(new MouseEvent('mousemove', base));
  el.dispatchEvent(new P('pointerdown', ptr({ buttons: 1 })));
  const go = el.dispatchEvent(new MouseEvent('mousedown', Object.assign({ buttons: 1, detail: 1 }, base)));
  if (go) {
    const fo = el.closest('a[href],button,input,select,textarea,summary,[tabindex],[contenteditable]') || el;
    try { if (fo.focus) fo.focus({ preventScroll: true }); } catch (_) {}
  }
  el.dispatchEvent(new P('pointerup', ptr({ buttons: 0 })));
  el.dispatchEvent(new MouseEvent('mouseup', Object.assign({ detail: 1 }, base)));
  if (typeof el.click === 'function') el.click();
  else el.dispatchEvent(new MouseEvent('click', Object.assign({ detail: 1 }, base)));
  return done(res);
}"#;

const TYPE: &str = r#"(a) => {
  const f = find(a.target, true);
  if (f.error) return f;
  const el = fieldOf(f.el);
  if (!el) return { error: desc(f.el) + ' is not a text field, select or editable area' };
  if (disabled(el)) return { error: desc(el) + ' is disabled' };
  if (el.readOnly === true) return { error: desc(el) + ' is read-only' };
  const text = String(a.text == null ? '' : a.text);
  const tag = el.tagName;
  if (tag === 'INPUT' && /^(checkbox|radio|button|submit|reset|image|file)$/i.test(el.type)) {
    return { error: desc(el) + ' is a ' + el.type + '; click it instead' };
  }
  arm(a);
  scrollTo(el);
  if (tag === 'SELECT') {
    const opts = Array.from(el.options), w = clean(text).toLowerCase();
    const o = opts.find((o) => o.value === text) || opts.find((o) => clean(o.text).toLowerCase() === w)
      || (w ? opts.find((o) => clean(o.text).toLowerCase().includes(w)) : null);
    if (!o) {
      return { error: 'no option "' + text + '" in ' + desc(el) + '; it has ' + opts.slice(0, 15).map((o) => '"' + clean(o.text, 40) + '"').join(', ') };
    }
    try { el.focus({ preventScroll: true }); } catch (_) {}
    setValue(el, o.value);
    return done({ selected: clean(o.text), value: o.value, in: desc(el) });
  }
  try { el.focus({ preventScroll: true }); } catch (_) {}
  const field = tag === 'INPUT' || tag === 'TEXTAREA';
  if (field) {
    const want = a.append ? el.value + text : text;
    let ok = false;
    try {
      if (a.append) { const n = el.value.length; try { el.setSelectionRange(n, n); } catch (_) {} }
      else { try { el.select(); } catch (_) {} }
      ok = text ? D.execCommand('insertText', false, text) : a.append || el.value === '' || D.execCommand('delete');
    } catch (_) {}
    if (!ok || el.value !== want) setValue(el, want);
    else el.dispatchEvent(new Event('change', { bubbles: true }));
  } else {
    const sel = W.getSelection(), range = D.createRange();
    range.selectNodeContents(el);
    if (a.append) range.collapse(false);
    sel.removeAllRanges();
    sel.addRange(range);
    let ok = false;
    try { ok = text ? D.execCommand('insertText', false, text) : D.execCommand('delete'); } catch (_) {}
    if (!ok) {
      el.textContent = a.append ? el.textContent + text : text;
      el.dispatchEvent(new Event('input', { bubbles: true, composed: true }));
    }
  }
  const res = { typed: desc(el) };
  res.value = tag === 'INPUT' && el.type === 'password' ? '(password)' : clean(field ? el.value : textOf(el), 200);
  if (a.enter) { const d = enter(el); if (d) res.enter = d; }
  return done(res);
}"#;

const KEY: &str = r#"(a) => {
  const parts = String(a.key || '').split('+').map((s) => s.trim()).filter(Boolean);
  if (!parts.length) return { error: 'which key? e.g. Enter, Escape, Tab, ArrowDown, Shift+Tab' };
  const mods = {};
  const MOD = { shift: 'shiftKey', ctrl: 'ctrlKey', control: 'ctrlKey', alt: 'altKey', option: 'altKey', opt: 'altKey', meta: 'metaKey', cmd: 'metaKey', command: 'metaKey' };
  for (const m of parts.slice(0, -1)) {
    const k = MOD[m.toLowerCase()];
    if (!k) return { error: 'unknown modifier ' + m + ' (Shift, Ctrl, Alt, Meta)' };
    mods[k] = true;
  }
  const name = parts[parts.length - 1];
  const k = KEYS[name.toLowerCase()] || (name.length === 1 ? [name, /[a-z]/i.test(name) ? 'Key' + name.toUpperCase() : /\d/.test(name) ? 'Digit' + name : name, name.toUpperCase().charCodeAt(0)] : null);
  if (!k) return { error: 'unknown key ' + name + ' (Enter, Escape, Tab, Backspace, Delete, Space, Arrow keys, Home, End, PageUp, PageDown, or one character)' };
  arm(a);
  const el = deepActive() || D.body || D.documentElement;
  const res = { key: (parts.length > 1 ? parts.slice(0, -1).join('+') + '+' : '') + k[0], to: el === D.body ? 'the page' : desc(el) };
  if (k[0] === 'Enter') { const d = enter(el, mods); if (d) res.did = d; return done(res); }
  const go = el.dispatchEvent(kev('keydown', k, mods));
  const plain = !mods.ctrlKey && !mods.metaKey && !mods.altKey;
  if (go && k[0].length === 1 && plain) el.dispatchEvent(kev('keypress', k, mods));
  if (go) {
    const edit = editable(el) || el.isContentEditable;
    const page = el === D.body || el === D.documentElement;
    if (k[0] === 'Tab') {
      const order = candidates((e) => e.tabIndex >= 0 && !disabled(e) && (e.matches(SEL) || e.tabIndex > 0), 5000);
      const pos = order.filter((e) => e.tabIndex > 0).sort((x, y) => x.tabIndex - y.tabIndex);
      const list = pos.concat(order.filter((e) => e.tabIndex === 0));
      if (list.length) {
        const i = list.indexOf(el);
        const next = list[mods.shiftKey ? (i <= 0 ? list.length - 1 : i - 1) : (i + 1) % list.length];
        try { next.focus(); } catch (_) {}
        res.focused = desc(next);
      }
    } else if (k[0] === ' ' && plain && !edit && (el.tagName === 'BUTTON' || el.getAttribute('role') === 'button' || (el.tagName === 'INPUT' && /^(checkbox|radio|button|submit)$/i.test(el.type)))) {
      el.click();
      res.did = 'activated';
    } else if (k[0] === 'Backspace' && edit) {
      try { D.execCommand('delete'); } catch (_) {}
    } else if (k[0] === 'Delete' && edit) {
      try { D.execCommand('forwardDelete'); } catch (_) {}
    } else if (k[0].length === 1 && plain && edit) {
      try { D.execCommand('insertText', false, k[0]); } catch (_) {}
    } else if (page || !edit) {
      const sc = scroller(), h = sc === D.scrollingElement ? innerHeight : sc.clientHeight;
      const dy = { ArrowDown: 40, ArrowUp: -40, PageDown: h * 0.85, PageUp: -h * 0.85, ' ': mods.shiftKey ? -h * 0.85 : h * 0.85 }[k[0]];
      if (dy && (page || k[0] === 'PageDown' || k[0] === 'PageUp')) sc.scrollTop += dy;
      if (k[0] === 'Home' && page) sc.scrollTop = 0;
      if (k[0] === 'End' && page) sc.scrollTop = sc.scrollHeight;
    }
  }
  el.dispatchEvent(kev('keyup', k, mods));
  return done(res);
}"#;

const SCROLL: &str = r#"(a) => {
  const to = String(a.to || 'down').trim(), dir = to.toLowerCase();
  if (!/^(up|down|top|bottom)$/.test(dir)) {
    const f = find(to, false, true);
    if (f.error) return f;
    scrollTo(f.el);
    return { scrolled_to: desc(f.el) };
  }
  const sc = scroller(), page = sc === D.scrollingElement || sc === D.documentElement;
  const h = page ? innerHeight : sc.clientHeight;
  const max = sc.scrollHeight - sc.clientHeight;
  const y = { up: sc.scrollTop - h * 0.85, down: sc.scrollTop + h * 0.85, top: 0, bottom: max }[dir];
  try { sc.scrollTo({ top: y, behavior: 'instant' }); } catch (_) { sc.scrollTop = y; }
  const now = Math.round(sc.scrollTop);
  return { scrolled: dir, in: page ? 'the page' : desc(sc), y: now, max: Math.round(max), at_end: now >= max - 2, at_top: now <= 0 };
}"#;

const NAV: &str = r#"(a) => {
  arm(a);
  const from = location.href;
  if (a.go === 'back') history.back();
  else if (a.go === 'forward') history.forward();
  else location.reload();
  return { [a.go]: true, from };
}"#;

/// One look at the page for `wait`: does it show the text, or the
/// selector, yet (or no longer, with `gone`)?
const WAIT: &str = r#"(a) => {
  const res = { url: location.href, title: D.title };
  let ok = false;
  if (a.selector) {
    let list;
    try { list = deepQuery(a.selector, true); } catch (e) { return { error: 'not a valid CSS selector: ' + a.selector }; }
    ok = list.some((el) => visible(el));
  } else {
    const alts = String(a.for || '').split('|').map((s) => s.trim()).filter(Boolean);
    const look = (s) => { s = String(s || '').toLowerCase(); return alts.find((n) => s.includes(n.toLowerCase())); };
    let hit = look(D.body ? D.body.innerText : '');
    // Text innerText leaves out (see `textOf`) and shadow roots: the visible
    // text nodes themselves.
    if (!hit) {
      const w = D.createTreeWalker(D.body || D.documentElement, 4);
      for (let n = w.nextNode(); n && !hit; n = w.nextNode()) {
        const p = n.parentElement;
        if (!p || /^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE)$/.test(p.tagName)) continue;
        const h = look(n.data);
        if (h && visible(p)) hit = h;
      }
    }
    if (!hit) {
      let shadow = '';
      each(D, (n) => { if (n.shadowRoot) shadow += ' ' + n.shadowRoot.textContent; });
      hit = look(shadow);
    }
    ok = !!hit;
    if (hit) res.matched = hit;
  }
  res.ok = a.gone ? !ok : ok;
  return res;
}"#;

/// The page as Markdown for a model to read: its main content (an
/// article, main, or the largest block of text), headings, paragraphs,
/// lists, quotes, code and links kept, menus, footers and hidden parts
/// left out. Open shadow roots are read too. Capped at `max` characters.
const READ: &str = r#"(a) => {
  const max = a.max || 40000;
  const skip = /^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE|SVG|CANVAS|IFRAME|NAV|FOOTER|ASIDE|FORM|BUTTON|SELECT|OPTION|DIALOG)$/;
  const hiddenEl = (el) => {
    if (el.hidden || el.getAttribute('aria-hidden') === 'true') return true;
    const cs = W.getComputedStyle(el);
    return cs.display === 'none' || cs.visibility === 'hidden';
  };
  const textLen = (el) => (el.innerText || el.textContent || '').length;
  let root = D.querySelector('article, main, [role=main], shreddit-post, #content') || D.body;
  // A page with several articles (a feed): their common parent.
  const arts = D.querySelectorAll('article, shreddit-post');
  if (arts.length > 1) root = D.querySelector('main, [role=main]') || D.body;
  if (root && textLen(root) < 400) root = D.body;
  const abs = (h) => { try { return new URL(h, location.href).href; } catch (e) { return h; } };
  let out = '';
  const block = (s) => { s = s.replace(/[ \t]+\n/g, '\n').trim(); if (s) out += (out ? '\n\n' : '') + s; };
  // Inline text of `el`: links as [text](href), code in backticks.
  const inline = (el) => {
    let s = '';
    const kids = el.shadowRoot ? [...el.shadowRoot.childNodes, ...el.childNodes] : el.childNodes;
    for (const n of kids) {
      if (n.nodeType === 3) { s += n.data.replace(/\s+/g, ' '); continue; }
      if (n.nodeType !== 1 || skip.test(n.tagName) || hiddenEl(n)) continue;
      const t = n.tagName;
      if (t === 'BR') s += '\n';
      else if (t === 'A' && n.getAttribute('href') && !n.getAttribute('href').startsWith('javascript')) {
        const txt = inline(n).trim();
        s += txt ? `[${txt}](${abs(n.getAttribute('href'))})` : '';
      } else if (t === 'CODE') s += '`' + n.textContent + '`';
      else if (t === 'STRONG' || t === 'B') { const x = inline(n).trim(); s += x ? `**${x}**` : ''; }
      else if (t === 'EM' || t === 'I') { const x = inline(n).trim(); s += x ? `*${x}*` : ''; }
      else if (t === 'IMG') { const alt = (n.getAttribute('alt') || '').trim(); if (alt) s += `[image: ${alt}]`; }
      else s += inline(n);
    }
    return s;
  };
  const BLOCK = /^(P|DIV|SECTION|ARTICLE|MAIN|HEADER|UL|OL|LI|H[1-6]|PRE|BLOCKQUOTE|TABLE|THEAD|TBODY|TFOOT|TR|DL|DT|DD|FIGURE|FIGCAPTION|DETAILS|SUMMARY|HR)$/;
  const hasBlock = (el) => [...el.children, ...(el.shadowRoot ? el.shadowRoot.children : [])].some((c) => BLOCK.test(c.tagName) || c.tagName.includes('-'));
  const walk = (el, depth) => {
    if (out.length > max) return;
    if (el.nodeType !== 1 || skip.test(el.tagName) || hiddenEl(el)) return;
    const t = el.tagName;
    if (/^H[1-6]$/.test(t)) { block('#'.repeat(+t[1]) + ' ' + inline(el).trim()); return; }
    if (t === 'PRE') { block('```\n' + el.innerText.replace(/\n+$/, '') + '\n```'); return; }
    if (t === 'HR') { block('---'); return; }
    if (t === 'BLOCKQUOTE') { block(inline(el).trim().split('\n').map((l) => '> ' + l).join('\n')); return; }
    if (t === 'LI') {
      const mark = el.parentElement && el.parentElement.tagName === 'OL' ? '1.' : '-';
      if (!hasBlock(el)) { block('  '.repeat(Math.max(0, depth)) + mark + ' ' + inline(el).trim()); return; }
    }
    if (t === 'TR') { block('| ' + [...el.children].map((c) => inline(c).trim().replace(/\|/g, '/')).join(' | ') + ' |'); return; }
    if (!hasBlock(el)) { const x = inline(el).trim(); if (x) block(x); return; }
    const kids = el.shadowRoot ? [...el.shadowRoot.children, ...el.children] : [...el.children];
    const nd = (t === 'UL' || t === 'OL') ? depth + 1 : depth;
    for (const c of kids) {
      if (c.tagName === 'SLOT') { for (const s of c.assignedElements()) walk(s, nd); continue; }
      walk(c, nd);
    }
    // Text sitting directly in a block that also has blocks.
    for (const n of el.childNodes) if (n.nodeType === 3 && n.data.trim().length > 30) block(n.data.trim());
  };
  walk(root, -1);
  out = out.replace(/\n{3,}/g, '\n\n');
  const cut = out.length > max;
  return { url: location.href, title: D.title, ready: D.readyState, chars: out.length,
           truncated: cut, markdown: cut ? out.slice(0, max) + '\n\n[… cut at ' + max + ' characters]' : out };
}"#;

/// How far the page has got: for `deck read`'s wait.
const READY: &str = r#"(a) => ({ ready: D.readyState, url: location.href, title: D.title,
  text: D.body ? (D.body.innerText || '').length : 0 })"#;

/// One step down an endless page, so the next batch loads.
const MORE: &str = r#"(a) => { const h = D.scrollingElement || D.body; const before = h.scrollHeight;
  W.scrollTo(0, h.scrollHeight); return { height: before }; }"#;

/// Where an action's navigation, if it started one, has got to.
const SETTLE: &str = r#"(a) => {
  const act = W.__deckAct && W.__deckAct.tok === a.tok ? W.__deckAct : null;
  return {
    same: W.__deckTok === a.tok, leaving: !!W.__deckLeaving, ready: D.readyState,
    url: location.href, title: D.title, from: act ? act.from : null,
    dialogs: act && act.dialogs && act.dialogs.length ? act.dialogs : null,
  };
}"#;

/// The script for `verb` with `args` (a JSON object, passed in as JSON).
pub fn script(verb: &str, args: &Value) -> Result<String, String> {
    let body = match verb {
        "snapshot" => SNAPSHOT,
        "click" => CLICK,
        "type" => TYPE,
        "key" | "press" => KEY,
        "scroll" => SCROLL,
        "back" | "forward" | "reload" => NAV,
        "wait" => WAIT,
        "settle" => SETTLE,
        "read" => READ,
        "ready" => READY,
        "more" => MORE,
        v => return Err(format!("no browser action {v:?}")),
    };
    let args = serde_json::to_string(args).map_err(|e| e.to_string())?;
    Ok(format!("(() => {{ {PRELUDE}\nreturn ({body})({args}); }})()"))
}

/// The pane an action is for: `pane` or `name`, or neither for the page
/// in the caller's tab.
fn target(args: &Value) -> Value {
    let mut t = json!({});
    for k in ["pane", "name"] {
        if let Some(v) = args.get(k).filter(|v| !v.is_null()) {
            t[k] = v.clone();
        }
    }
    t
}

fn eval(target: &Value, js: &str) -> Result<Value, String> {
    let mut p = target.clone();
    p["js"] = js.into();
    p["expr"] = true.into();
    let v = control::call("browser.eval", p)?;
    Ok(v.get("value").cloned().unwrap_or(Value::Null))
}

/// An eval that failed because the page was between documents, rather
/// than because there is no such pane.
fn transient(e: &str) -> bool {
    e.contains("did not run it") || e.contains("no answer") || e.contains("bad eval result")
        || e.contains("did not answer")
}

fn token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{:x}{:x}", nanos, std::process::id())
}

/// Run `verb` against a page: `snapshot`, `click`, `type`, `key`,
/// `scroll`, `back`, `forward`, `reload` or `wait`. `args` holds the
/// pane (`pane` or `name`) and the verb's own fields.
pub fn run(verb: &str, args: Value) -> Result<Value, String> {
    let target = target(&args);
    if verb == "wait" {
        return wait(&target, &args);
    }
    if verb == "read" {
        return read_here(&target, &args);
    }
    let mut a = args.clone();
    let tok = token();
    a["tok"] = tok.clone().into();
    if matches!(verb, "back" | "forward" | "reload") {
        a["go"] = verb.into();
    }
    let mut v = eval(&target, &script(verb, &a)?)?;
    if let Some(e) = v.get("error").and_then(Value::as_str) {
        return Err(e.to_string());
    }
    // How long a page that has not started to navigate gets before the
    // action counts as done where it is.
    let quiet = match verb {
        "click" | "key" | "press" => Some(400),
        "type" if args.get("enter").and_then(Value::as_bool) == Some(true) => Some(400),
        "back" | "forward" | "reload" => Some(1500),
        _ => None,
    };
    if let Some(quiet) = quiet {
        let from = v.get("from").and_then(Value::as_str).map(str::to_string);
        let s = settle(&target, &tok, Duration::from_millis(quiet));
        if let (Some(obj), Some(s)) = (v.as_object_mut(), s.as_object()) {
            for k in ["url", "title", "navigated", "loading", "dialogs"] {
                if let Some(x) = s.get(k).filter(|x| !x.is_null()) {
                    obj.insert(k.into(), x.clone());
                }
            }
            if from.is_some() && obj.get("navigated") == Some(&Value::Bool(false)) {
                obj.insert("note".into(), "nothing to go to".into());
            }
        }
    }
    Ok(v)
}

/// Follow what an action set off: until the page it left has been
/// replaced and has loaded, or, when it did not navigate, for `quiet`.
fn settle(target: &Value, tok: &str, quiet: Duration) -> Value {
    let js = match script("settle", &json!({ "tok": tok })) {
        Ok(js) => js,
        Err(_) => return Value::Null,
    };
    let start = Instant::now();
    let limit = Duration::from_secs(15);
    let mut last = Value::Null;
    loop {
        std::thread::sleep(Duration::from_millis(150));
        if let Ok(v) = eval(target, &js) {
            let same = v["same"] == true;
            let moved = v["from"].as_str().is_some_and(|f| Some(f) != v["url"].as_str());
            let ready = v["ready"] == "complete"
                || (v["ready"] == "interactive" && start.elapsed() > Duration::from_secs(4));
            last = v;
            if !same && ready {
                return json!({ "url": last["url"], "title": last["title"], "navigated": true, "dialogs": last["dialogs"] });
            }
            if same && last["leaving"] != true && start.elapsed() >= quiet {
                return json!({ "url": last["url"], "title": last["title"], "navigated": moved, "dialogs": last["dialogs"] });
            }
        }
        if start.elapsed() > limit {
            return json!({ "url": last["url"], "title": last["title"], "navigated": last["same"] == false, "loading": true, "dialogs": last["dialogs"] });
        }
    }
}

/// Poll the page every 300 ms until it shows the text or selector (or no
/// longer does, with `gone`). On timeout the error names the page it is on.
fn wait(target: &Value, args: &Value) -> Result<Value, String> {
    let want_text = args.get("for").and_then(Value::as_str).filter(|s| !s.trim().is_empty());
    let want_sel = args.get("selector").and_then(Value::as_str).filter(|s| !s.trim().is_empty());
    if want_text.is_none() && want_sel.is_none() {
        return Err("wait needs --for TEXT or --selector SEL".into());
    }
    let timeout = args
        .get("timeout")
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
        .unwrap_or(30)
        .clamp(1, 1800);
    let gone = args.get("gone").and_then(Value::as_bool).unwrap_or(false);
    let js = script("wait", &json!({ "for": want_text, "selector": want_sel, "gone": gone }))?;
    let what = match (want_sel, gone) {
        (Some(s), false) => format!("{s:?} on the page"),
        (Some(s), true) => format!("{s:?} gone from the page"),
        (None, false) => format!("{:?} on the page", want_text.unwrap_or_default()),
        (None, true) => format!("{:?} gone from the page", want_text.unwrap_or_default()),
    };
    let start = Instant::now();
    let mut last = Value::Null;
    loop {
        match eval(target, &js) {
            Ok(v) => {
                if let Some(e) = v.get("error").and_then(Value::as_str) {
                    return Err(e.to_string());
                }
                if v["ok"] == true {
                    let mut out = json!({ "url": v["url"], "title": v["title"], "seconds": start.elapsed().as_secs() });
                    if let Some(m) = v.get("matched") {
                        out["matched"] = m.clone();
                    }
                    return Ok(out);
                }
                last = v;
            }
            Err(e) if !transient(&e) => return Err(e),
            Err(_) => {}
        }
        if start.elapsed().as_secs() >= timeout {
            let page = match (last["title"].as_str(), last["url"].as_str()) {
                (Some(t), Some(u)) if !t.is_empty() => format!("{t:?} at {u}"),
                (_, Some(u)) => u.to_string(),
                _ => "not answering".into(),
            };
            return Err(format!("no {what} after {timeout}s; the page is {page}"));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_take_arguments_as_json() {
        let js = script("click", &json!({ "target": "text=\"); alert(1); (\"", "tok": "t" })).unwrap();
        // The target reaches the page as one JSON string, quotes escaped.
        assert!(js.contains(r#"{"target":"text=\"); alert(1); (\"","tok":"t"}"#));
        assert!(js.starts_with("(() => {") && js.ends_with("})()"));
        for verb in ["snapshot", "click", "type", "key", "scroll", "back", "wait", "settle"] {
            assert!(script(verb, &json!({})).is_ok(), "{verb}");
        }
        assert!(script("hover", &json!({})).is_err());
    }

    #[test]
    fn targets_keep_only_the_pane() {
        assert_eq!(target(&json!({ "pane": 3, "target": "x" })), json!({ "pane": 3 }));
        assert_eq!(target(&json!({ "name": "app", "text": "x" })), json!({ "name": "app" }));
        assert_eq!(target(&json!({ "pane": null })), json!({}));
    }

    /// `DECK_BROWSE_JS=DIR cargo test -p app dump_browse_scripts -- --ignored`
    /// writes each action's script to DIR, to try in a browser.
    #[test]
    #[ignore]
    fn dump_browse_scripts() {
        let Some(dir) = std::env::var_os("DECK_BROWSE_JS") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("recorder.js"), crate::browser::RECORDER_SCRIPT).unwrap();
        std::fs::write(dir.join("prelude.js"), PRELUDE).unwrap();
        for (name, body) in [
            ("snapshot", SNAPSHOT),
            ("click", CLICK),
            ("type", TYPE),
            ("key", KEY),
            ("scroll", SCROLL),
            ("nav", NAV),
            ("wait", WAIT),
            ("settle", SETTLE),
        ] {
            std::fs::write(dir.join(format!("{name}.js")), body).unwrap();
        }
    }
}

/// The page in `target` as Markdown, after scrolling `scroll` times to load
/// more of an endless page.
fn read_here(target: &Value, args: &Value) -> Result<Value, String> {
    let num = |k: &str| args.get(k).and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()));
    for _ in 0..num("scroll").unwrap_or(0).min(30) {
        eval(target, &script("more", &json!({}))?)?;
        std::thread::sleep(Duration::from_millis(1200));
    }
    let v = eval(target, &script("read", &json!({ "max": num("max").unwrap_or(40_000) }))?)?;
    if v.is_null() {
        return Err("the page gave no answer".into());
    }
    Ok(v)
}

/// `deck read URL`: open URL in a background page (a real WebKit page with
/// Deck's user agent and your sign-ins, out of sight), wait for it to load
/// and settle, and return it as Markdown. The page stays open under its
/// name, so `deck browser click --name …` and friends can go on from there.
pub fn read_url(args: &Value) -> Result<Value, String> {
    let url = args.get("url").and_then(Value::as_str).ok_or("read needs a URL")?;
    let name = args.get("name").and_then(Value::as_str).unwrap_or("reader").to_string();
    let opened = control::call("browser.open", json!({ "url": url, "name": name, "bg": true }))?;
    let target = json!({ "pane": opened["pane"] });
    let timeout = args
        .get("timeout")
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
        .unwrap_or(30);
    let start = Instant::now();
    let ready_js = script("ready", &json!({}))?;
    // Past the old document (a reused page) and loaded; a page that keeps
    // loading counts once it has text and four seconds have gone.
    std::thread::sleep(Duration::from_millis(400));
    loop {
        if let Ok(v) = eval(&target, &ready_js) {
            let loaded = v["ready"] == "complete" || (v["ready"] == "interactive" && start.elapsed() > Duration::from_secs(4));
            let real = v["url"].as_str().is_some_and(|u| u != "about:blank");
            if loaded && real && v["text"].as_u64().unwrap_or(0) > 0 {
                break;
            }
        }
        if start.elapsed() > Duration::from_secs(timeout) {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    // Client-side apps render after load.
    std::thread::sleep(Duration::from_millis(1200));
    let mut v = read_here(&target, args)?;
    v["pane"] = opened["pane"].clone();
    v["name"] = name.into();
    Ok(v)
}
