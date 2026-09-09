/* Execution tests for web/app.js.
 *
 * test-themes.js checks wiring statically; this file goes further and runs
 * the REAL app.js — init, event handlers, the sign-in flow and the first-run
 * sign-up flow — against a stub DOM and a stub server. It exists because a
 * whole class of shipped bugs is invisible to static checks: the code parses,
 * the elements exist, and everything still fails silently in the browser.
 *
 * Run it with no dependencies and no build step:
 *
 *   node web/test-app.js
 *
 * The most valuable scenario is the "stale page" one: a browser running a
 * newer app.js against a cached older index.html. That exact combination made
 * both the sign-in and sign-up buttons die without any error.
 */

'use strict';

const fs = require('fs');
const path = require('path');

const themesSource = fs.readFileSync(path.join(__dirname, 'themes.js'), 'utf8');
const appSource = fs.readFileSync(path.join(__dirname, 'app.js'), 'utf8');

/* ---------------------------------------------------------- DOM stubs */

let NEXT_EL_ID = 1;

function makeEl(sel) {
  return {
    __elId: NEXT_EL_ID++,
    sel,
    hidden: false,
    value: '',
    textContent: '',
    innerHTML: '',
    placeholder: '',
    disabled: false,
    checked: false,
    dataset: {},
    style: {
      setProperty() {},
      removeProperty() {},
      getPropertyValue() { return ''; },
    },
    classList: {
      add() {}, remove() {}, toggle() {}, contains() { return false; },
    },
    events: {},
    children: [],
    addEventListener(type, fn) { (this.events[type] = this.events[type] || []).push(fn); },
    removeEventListener() {},
    appendChild(child) { this.children.push(child); return child; },
    append(...kids) { for (const k of kids) this.children.push(k); },
    setAttribute(k, v) { this[k] = v; },
    getAttribute() { return null; },
    removeAttribute() {},
    querySelector() { return makeEl('nested'); },
    querySelectorAll() { return []; },
    focus() {},
    fire(type) {
      for (const fn of this.events[type] || []) {
        fn({ preventDefault() {}, target: this, dataTransfer: null, key: '' });
      }
    },
  };
}

// Ids present in the CURRENT index.html. The "stale page" scenario removes
// the sign-up ones, simulating a browser that cached an older markup.
const SIGNUP_IDS = ['signin-fields', 'signup-fields', 'to-signup', 'to-signin',
  'signup-server', 'signup-username', 'signup-password', 'signup-confirm',
  'signup-vault', 'signup-submit'];

function makeDom({ includeSignup }) {
  const map = new Map();
  // byId accepts an id with or without the leading '#'; both must land on the
  // same element or the assertions would read fresh, disconnected stubs.
  const get = (sel) => {
    if (sel.startsWith('#') && SIGNUP_IDS.includes(sel.slice(1)) && !includeSignup) {
      return null; // this element does not exist on a stale (pre-sign-up) page
    }
    if (!map.has(sel)) map.set(sel, makeEl(sel));
    return map.get(sel);
  };
  return {
    map,
    get,
    byId: (id) => get(id.startsWith('#') ? id : '#' + id),
  };
}

function makeStorage() {
  const map = new Map();
  return {
    map,
    getItem(k) { return map.has(k) ? map.get(k) : null; },
    setItem(k, v) { map.set(k, String(v)); },
    removeItem(k) { map.delete(k); },
  };
}

function makeFetch(calls, { firstRun }) {
  return function fetch(url) {
    url = String(url);
    calls.push(url);
    if (process.env.QD_DEBUG && url.includes('/auth/')) {
      console.log(`  [debug] fetch ${url.replace(/^https?:\/\/[^/]+/, '')}`);
      console.log(new Error().stack.split('\n').slice(1, 6).join('\n'));
    }
    let body = {};
    if (url.includes('/auth/status')) body = { first_run: firstRun };
    else if (url.includes('/auth/login')) body = { token: 'tok-login', user_id: 'u1' };
    else if (url.includes('/auth/register')) body = { token: 'tok-reg', user_id: 'u2', vault: 'myvault' };
    else if (url.includes('/whoami')) body = { user: 'ada' };
    else if (url.includes('/timeline')) body = { items: [] };
    else if (url.includes('/stats')) body = { files: 0, dirs: 1, symlinks: 0, bytes: 0 };
    else if (url.includes('/fs')) body = { path: '', entries: [] };
    else if (url.includes('/vaults')) body = { vaults: [{ name: 'myvault', encrypted: false }] };
    return Promise.resolve({
      ok: true,
      status: 200,
      json: () => Promise.resolve(body),
    });
  };
}

async function boot({ includeSignup = true, firstRun = false } = {}) {
  const calls = [];
  const dom = makeDom({ includeSignup });
  const storage = makeStorage();
  let domReady = null;

  const documentStub = {
    documentElement: makeEl('html'),
    body: makeEl('body'),
    querySelector: (sel) => dom.get(sel),
    getElementById: (id) => dom.byId(id),
    createElement: (tag) => makeEl(tag),
    createTextNode: (text) => ({ text }),
    addEventListener(type, fn) { if (type === 'DOMContentLoaded') domReady = fn; },
  };
  const windowStub = {
    location: { origin: 'http://stub:8787' },
    prompt() { return null; },
    confirm() { return false; },
    addEventListener() {},
  };
  const URLStub = { createObjectURL() { return 'blob:stub'; }, revokeObjectURL() {} };

  const run = (source) => new Function(
    'window', 'document', 'localStorage', 'fetch', 'URL', 'QuarkdriveThemes', source,
  )(windowStub, documentStub, storage, makeFetch(calls, { firstRun }), URLStub,
    windowStub.QuarkdriveThemes);

  run(themesSource); // themes.js first, as index.html loads it
  run(appSource);

  if (!domReady) throw new Error('app.js never registered a DOMContentLoaded handler');
  await domReady(); // init() is synchronous; a throw here means broken wiring
  const tick = () => new Promise((r) => setTimeout(r, 10));
  await tick();
  await tick();

  return { dom, calls, storage };
}

/* --------------------------------------------------------- assertions */

let passed = 0;
let failed = 0;

function check(label, condition, detail) {
  if (condition) {
    passed += 1;
    console.log(`  ok    ${label}`);
  } else {
    failed += 1;
    console.log(`  FAIL  ${label}${detail ? ` — ${detail}` : ''}`);
  }
}

function section(name) {
  console.log(`\n${name}`);
}

const tick = () => new Promise((r) => setTimeout(r, 25));

async function main() {
  section('boot on a normal server (accounts exist)');

  let boot1;
  try {
    boot1 = await boot({ includeSignup: true, firstRun: false });
    check('init() runs without throwing', true);
  } catch (e) {
    check('init() runs without throwing', false, e.message);
    return finish();
  }
  {
    const { dom } = boot1;
    check('login view is shown', dom.byId('#login-view').hidden === false);
    check('main view is hidden', dom.byId('#main-view').hidden === true);
    check('sign-in card is the default', dom.byId('#signin-fields').hidden === false);
    check('sign-up card stays hidden on a normal server',
      dom.byId('#signup-fields').hidden === true);
  }

  section('sign in end to end');

  {
    const { dom, calls } = boot1;
    dom.byId('#login-server').value = 'http://srv:8787';
    dom.byId('#login-username').value = 'ada';
    dom.byId('#login-password').value = 'hunter22';
    dom.byId('#login-form').fire('submit');
    await tick();
    check('the login endpoint was called',
      calls.some((u) => u.includes('/auth/login')));
    check('the app view is shown after sign in',
      dom.byId('#main-view').hidden === false);
    check('the login view is hidden after sign in',
      dom.byId('#login-view').hidden === true);
    check('the vault list was populated',
      dom.byId('#vault-select').children.some((o) => o.value === 'myvault'));
  }

  section('first-run sign up end to end');

  {
    const { dom, calls, storage } = await boot({ includeSignup: true, firstRun: true });
    check('a fresh server opens on the sign-up card',
      dom.byId('#signup-fields').hidden === false &&
      dom.byId('#signin-fields').hidden === true);
    dom.byId('#signup-server').value = 'http://srv:8787';
    dom.byId('#signup-username').value = 'grace';
    dom.byId('#signup-password').value = 'hunter22';
    dom.byId('#signup-confirm').value = 'hunter22';
    dom.byId('#signup-vault').value = 'photos';
    dom.byId('#login-form').fire('submit');
    await tick();
    check('the register endpoint was called',
      calls.some((u) => u.includes('/auth/register')));
    check('a session token was stored', storage.getItem('qd.token') === 'tok-reg');
    check('the vault from registration was stored',
      storage.getItem('qd.vault') === 'myvault');
    check('the app view is shown after registering',
      dom.byId('#main-view').hidden === false);
  }

  section('sign-up validation');

  {
    const { dom, calls } = await boot({ includeSignup: true, firstRun: true });
    dom.byId('#signup-password').value = 'abc';
    dom.byId('#signup-confirm').value = 'abc';
    dom.byId('#login-form').fire('submit');
    await tick();
    check('a short password is refused with a visible message',
      dom.byId('#login-error').hidden === false &&
      /6 characters/.test(dom.byId('#login-error').textContent));
    check('nothing was sent to the server',
      !calls.some((u) => u.includes('/auth/register')));

    dom.byId('#signup-password').value = 'hunter22';
    dom.byId('#signup-confirm').value = 'different';
    dom.byId('#login-form').fire('submit');
    await tick();
    check('mismatched passwords are refused',
      /do not match/.test(dom.byId('#login-error').textContent));
    check('still nothing was sent to the server',
      !calls.some((u) => u.includes('/auth/register')));
  }

  section('stale page: new script against cached old markup');

  {
    // The exact combination that shipped: app.js referencing sign-up
    // elements that a cached older index.html does not have. Sign-in must
    // still work, with no silent failure.
    let bootErr = null;
    let ctx;
    try {
      ctx = await boot({ includeSignup: false, firstRun: false });
    } catch (e) {
      bootErr = e;
    }
    check('init() survives missing sign-up elements', bootErr === null,
      bootErr && bootErr.message);
    if (!bootErr) {
      const { dom, calls } = ctx;
      dom.byId('#login-server').value = 'http://srv:8787';
      dom.byId('#login-username').value = 'ada';
      dom.byId('#login-password').value = 'hunter22';
      let submitErr = null;
      try {
        dom.byId('#login-form').fire('submit');
        await tick();
      } catch (e) {
        submitErr = e;
      }
      check('submitting sign-in on a stale page does not throw', submitErr === null,
        submitErr && submitErr.message);
      check('sign in still completes on a stale page',
        calls.some((u) => u.includes('/auth/login')) &&
        dom.byId('#main-view').hidden === false);
    }
  }

  return finish();
}

function finish() {
  console.log(`\n${passed} passed, ${failed} failed`);
  process.exit(failed ? 1 : 0);
}

main();
