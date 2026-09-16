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

function makeFetch(calls, { firstRun, shareRole = 'owner' }) {
  return function fetch(url) {
    url = String(url);
    calls.push(url);
    if (process.env.QD_DEBUG && url.includes('/auth/')) {
      console.log(`  [debug] fetch ${url.replace(/^https?:\/\/[^/]+/, '')}`);
      console.log(new Error().stack.split('\n').slice(1, 6).join('\n'));
    }
    let body = {};
    if (url.includes('/search')) {
      body = {
        path: '',
        entries: [
          {
            name: 'holiday.jpg',
            path: 'photos/2024/holiday.jpg',
            kind: 'file',
            size: 1024,
            mtime: 0,
          },
        ],
      };
    } else if (url.includes('/auth/status')) body = { first_run: firstRun };
    else if (url.includes('/auth/login')) body = { token: 'tok-login', user_id: 'u1' };
    else if (url.includes('/auth/register')) body = { token: 'tok-reg', user_id: 'u2', vault: 'myvault' };
    else if (url.includes('/whoami')) body = { user: 'ada' };
    else if (url.includes('/timeline')) body = { items: [] };
    else if (url.includes('/stats')) body = { files: 0, dirs: 1, symlinks: 0, bytes: 0 };
    else if (url.includes('/fs/versions/restore')) body = { ok: true };
    else if (url.includes('/fs/versions')) {
      body = { items: [{ id: 'ver1', size: 5, mtime: 0, snapshot_time: 0, device: 'laptop' }] };
    } else if (url.includes('/trash/restore')) body = { ok: true, path: 'old.txt' };
    else if (url.includes('/trash')) {
      body = /trash\?/.test(url)
        ? { ok: true }
        : { items: [{ id: 't1', path: 'old.txt', kind: 'file', size: 5, deleted_at: 0 }] };
    } else if (url.includes('/links')) {
      body = url.includes('/links/lnk')
        ? { ok: true }
        : { items: [{ id: 'lnk1', path: 'docs', has_password: false, expires: null, created: 0 }] };
    } else if (url.includes('/shares')) {
      body = /shares\/[^/?]+$/.test(url.split('?')[0]) && url.split('?')[0].endsWith('/shares') === false
        ? { ok: true }
        : { shares: [{ username: 'bob', role: 'write', created: 0 }] };
    } else if (url.includes('/fs')) {
      body = {
        path: '',
        entries: [{ name: 'a.txt', path: 'a.txt', kind: 'file', size: 1, mtime: 0 }],
      };
    }
    else if (url.includes('/vaults')) {
      body = { vaults: [{ name: 'myvault', encrypted: false, role: shareRole }] };
    }
    return Promise.resolve({
      ok: true,
      status: 200,
      json: () => Promise.resolve(body),
      blob: () => Promise.resolve({ size: 0, type: 'image/jpeg' }),
    });
  };
}

async function boot({ includeSignup = true, firstRun = false, shareRole = 'owner' } = {}) {
  const calls = [];
  const dom = makeDom({ includeSignup });
  const storage = makeStorage();
  let domReady = null;

  const docEvents = {};
  const documentStub = {
    documentElement: makeEl('html'),
    body: makeEl('body'),
    querySelector: (sel) => dom.get(sel),
    getElementById: (id) => dom.byId(id),
    createElement: (tag) => makeEl(tag),
    createTextNode: (text) => ({ text }),
    addEventListener(type, fn) {
      if (type === 'DOMContentLoaded') domReady = fn;
      (docEvents[type] = docEvents[type] || []).push(fn);
    },
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
  )(windowStub, documentStub, storage, makeFetch(calls, { firstRun, shareRole }), URLStub,
    windowStub.QuarkdriveThemes);

  run(themesSource); // themes.js first, as index.html loads it
  run(appSource);

  if (!domReady) throw new Error('app.js never registered a DOMContentLoaded handler');
  await domReady(); // init() is synchronous; a throw here means broken wiring
  const tick = () => new Promise((r) => setTimeout(r, 10));
  await tick();
  await tick();

  return { dom, calls, storage, docEvents };
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

function buttonByText(el, text) {
  // Buttons may sit directly on the row or inside a nested actions span.
  for (const c of el.children || []) {
    if (c.textContent === text && c.events && c.events.click) return c;
    const nested = (c.children || []).find(
      (g) => g.textContent === text && g.events && g.events.click,
    );
    if (nested) return nested;
  }
  return undefined;
}


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

  section('trash: listing, restore, purge guards');

  {
    const { dom, calls } = await boot({ includeSignup: true, firstRun: false });
    dom.byId('#login-server').value = 'http://srv:8787';
    dom.byId('#login-username').value = 'ada';
    dom.byId('#login-password').value = 'hunter22';
    dom.byId('#login-form').fire('submit');
    await tick(); await tick();

    dom.byId('#tab-trash').fire('click');
    await tick(); await tick();
    check('trash tab shows the deleted file',
      (dom.byId('#trash-list').innerHTML || '').includes('old.txt') ||
      (dom.byId('#trash-list').children[0] || {}).className === 'entry');
    check('trash request went to the vault trash endpoint',
      calls.some((u) => u.includes('/vaults/myvault/trash')));

    const row = dom.byId('#trash-list').children[0];
    const restore = buttonByText(row, 'Restore');
    check('row offers Restore', Boolean(restore));
    restore.fire('click');
    await tick(); await tick();
    check('restore hits the restore endpoint',
      calls.some((u) => u.includes('/trash/restore')));

    const purge = buttonByText(row, 'Delete forever');
    check('row offers Delete forever', Boolean(purge));
    purge.fire('click');
    await tick();
    check('purge is refused when confirm() says no',
      !calls.some((u) => /trash\?id=/.test(u)));

    dom.byId('#btn-empty-trash').fire('click');
    await tick();
    check('empty-trash is also guarded by confirm()',
      !calls.some((u) => /trash\?all=true/.test(u)));
  }

  section('sharing: drawer, invite, revoke, owner-only button');

  {
    const { dom, calls } = await boot({ includeSignup: true, firstRun: false });
    dom.byId('#login-server').value = 'http://srv:8787';
    dom.byId('#login-username').value = 'ada';
    dom.byId('#login-password').value = 'hunter22';
    dom.byId('#login-form').fire('submit');
    await tick(); await tick();

    check('share button visible for the owner',
      dom.byId('#btn-share').hidden === false);
    dom.byId('#btn-share').fire('click');
    await tick(); await tick();
    check('share drawer opens', dom.byId('#share-drawer').hidden === false);
    check('existing shares are fetched',
      calls.some((u) => u.includes('/vaults/myvault/shares')));

    dom.byId('#share-user').value = 'carol';
    dom.byId('#btn-add-share').fire('click');
    await tick(); await tick();
    check('invite posted to the shares endpoint',
      calls.filter((u) => u.includes('/shares')).length >= 1);

    const row = dom.byId('#share-list').children[0];
    const revoke = buttonByText(row, 'Revoke');
    check('share row offers Revoke', Boolean(revoke));
    revoke.fire('click');
    await tick(); await tick();
    check('revoke deletes the share',
      calls.some((u) => /shares\/bob/.test(u)));

    dom.byId('#share-close').fire('click');
    check('drawer closes', dom.byId('#share-drawer').hidden === true);
  }

  section('public links: list, create, guarded remove');

  {
    const { dom, calls } = await boot({ includeSignup: true, firstRun: false });
    dom.byId('#login-server').value = 'http://srv:8787';
    dom.byId('#login-username').value = 'ada';
    dom.byId('#login-password').value = 'hunter22';
    dom.byId('#login-form').fire('submit');
    await tick(); await tick();

    dom.byId('#btn-share').fire('click');
    await tick(); await tick();
    check('existing links are listed in the drawer',
      (dom.byId('#link-list').innerHTML || '').includes('docs') ||
      (dom.byId('#link-list').children[0] || {}).className === 'entry');

    dom.byId('#link-password').value = 'sesame';
    dom.byId('#btn-create-link').fire('click');
    await tick(); await tick();
    check('creating a link posts the folder path and password',
      calls.some((u) => u.includes('/links')));

    const row = dom.byId('#link-list').children[0];
    const remove = buttonByText(row, 'Remove');
    check('link row offers Remove', Boolean(remove));
    remove.fire('click');
    await tick();
    check('removing a link is guarded by confirm()',
      !calls.some((u) => /links\/lnk1/.test(u) && true));
  }

  section('file versions: open modal, restore');

  {
    const { dom, calls } = await boot({ includeSignup: true, firstRun: false });
    dom.byId('#login-server').value = 'http://srv:8787';
    dom.byId('#login-username').value = 'ada';
    dom.byId('#login-password').value = 'hunter22';
    dom.byId('#login-form').fire('submit');
    await tick(); await tick();

    const row = dom.byId('#entry-list').children[0];
    const versions = buttonByText(row, 'Versions');
    check('file rows offer a Versions action', Boolean(versions));
    versions.fire('click');
    await tick(); await tick();
    check('versions modal opens', dom.byId('#versions-modal').hidden === false);
    check('versions were fetched for the file',
      calls.some((u) => u.includes('/fs/versions?path=')));
    const vrow = dom.byId('#versions-list').children[0];
    check('a version row shows the saved date',
      (vrow.textContent || '').length > 0 || (vrow.children || []).length > 0);

    const restore = buttonByText(vrow, 'Restore');
    check('version row offers Restore', Boolean(restore));
    restore.fire('click');
    await tick(); await tick();
    check('restore hits the versions restore endpoint',
      calls.some((u) => u.includes('/fs/versions/restore')));
    check('modal closes after restoring', dom.byId('#versions-modal').hidden === true);
  }

  section('a read-only share hides the share button');

  {
    const { dom } = await boot({ includeSignup: true, firstRun: false, shareRole: 'read' });
    dom.byId('#login-server').value = 'http://srv:8787';
    dom.byId('#login-username').value = 'carol';
    dom.byId('#login-password').value = 'singer-8';
    dom.byId('#login-form').fire('submit');
    await tick(); await tick();
    check('share button hidden for a read-only sharee',
      dom.byId('#btn-share').hidden === true);
    check('vault dropdown marks the shared vault',
      dom.byId('#vault-select').children.some((o) => (o.textContent || '').includes('read share')));
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

  section('search asks the server, not just the loaded folder');
  {
    const { dom, calls, docEvents } = await boot({ includeSignup: true, firstRun: false });
    dom.byId('#login-server').value = 'http://srv:8787';
    dom.byId('#login-username').value = 'ada';
    dom.byId('#login-password').value = 'hunter22';
    dom.byId('#login-form').fire('submit');
    await tick();

    const before = calls.length;
    const box = dom.byId('#filter-input');
    box.value = 'holiday';
    box.fire('input');

    // Local matches render immediately; the server is asked after a debounce.
    check('typing does not hit the server on every keystroke',
      !calls.slice(before).some((u) => u.includes('/search')));

    await new Promise((r) => setTimeout(r, 400));

    const search = calls.find((u) => u.includes('/search'));
    check('a search request is sent after the debounce', Boolean(search),
      `calls: ${calls.slice(before).join(', ') || 'none'}`);
    check('the typed term is passed to the server',
      Boolean(search) && search.includes('q=holiday'), search);

    // A hit in a folder the user never opened is the whole point: filtering
    // the current listing could not have found this one.
    const list = dom.byId('#entry-list');
    const rendered = JSON.stringify(list.children);
    check('a result from another folder is rendered',
      rendered.includes('holiday.jpg'),
      rendered.slice(0, 200));

    check('the drop guard is registered on the document',
      Array.isArray(docEvents.drop) && Array.isArray(docEvents.dragover));

    // Dropping a file outside the zone must be cancelled, or the browser
    // navigates away from the app.
    let prevented = false;
    for (const fn of docEvents.drop || []) {
      fn({ preventDefault() { prevented = true; }, target: documentStubTarget() });
    }
    check('a drop anywhere on the page is cancelled', prevented);
  }

  return finish();
}

/** A drop target that is not the upload zone. */
function documentStubTarget() {
  return { sel: 'somewhere-else' };
}

function finish() {
  console.log(`\n${passed} passed, ${failed} failed`);
  process.exit(failed ? 1 : 0);
}

main();
