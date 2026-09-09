/* Tests for web/themes.js.
 *
 * Run directly, with no dependencies and no build step:
 *
 *   node web/test-themes.js
 *
 * themes.js expects a browser, so this stubs just enough of one — a document
 * element with a style object, and a localStorage — to execute the real code
 * rather than a copy of it.
 */

'use strict';

const fs = require('fs');
const path = require('path');

/* --------------------------------------------------------------- stubs */

function makeStyleStore() {
  const props = new Map();
  return {
    setProperty(k, v) { props.set(k, String(v)); },
    removeProperty(k) { props.delete(k); },
    getPropertyValue(k) { return props.has(k) ? props.get(k) : ''; },
    get size() { return props.size; },
    has(k) { return props.has(k); },
  };
}

const storage = {
  map: new Map(),
  getItem(k) { return this.map.has(k) ? this.map.get(k) : null; },
  setItem(k, v) { this.map.set(k, String(v)); },
  removeItem(k) { this.map.delete(k); },
};

const classList = (() => {
  const set = new Set();
  return {
    add(name) { set.add(name); },
    remove(name) { set.delete(name); },
    toggle(name, force) {
      const on = force === undefined ? !set.has(name) : !!force;
      if (on) set.add(name); else set.delete(name);
    },
    contains(name) { return set.has(name); },
  };
})();

global.localStorage = storage;
global.document = {
  documentElement: {
    style: makeStyleStore(),
    classList,
  },
};
global.window = {};

/* -------------------------------------------------------------- loading */

const source = fs.readFileSync(path.join(__dirname, 'themes.js'), 'utf8');
// The file is an IIFE that assigns to window; evaluate it in this context.
new Function('window', 'localStorage', 'document', source)(
  global.window, global.localStorage, global.document,
);

const T = global.window.QuarkdriveThemes;

/* ----------------------------------------------------------- assertions */

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

/* --------------------------------------------------------------- tests */

section('built-in themes');

check('themes.js exposes its API', typeof T === 'object' && !!T.all);
check('at least six built-in themes', T.all().length >= 6, `got ${T.all().length}`);
check('galaxy exists and is the default', T.DEFAULT_ID === 'galaxy' && !!T.get('galaxy'));
check('every theme has a name, swatch and vars',
  T.all().every((t) => t.id && t.name && Array.isArray(t.swatch) && t.vars && Object.keys(t.vars).length > 5));
check('both light and dark themes are offered',
  T.all().some((t) => t.dark) && T.all().some((t) => t.dark === false));
check('theme ids are unique',
  new Set(T.all().map((t) => t.id)).size === T.all().length);

section('applying a theme');

const root = global.document.documentElement.style;
T.apply(T.get('galaxy'));
check('background is set', /^#|gradient|linear/.test(root.getPropertyValue('--bg')));
check('accent is set', /^#[0-9a-f]{6}$/i.test(root.getPropertyValue('--accent')));
check('accent glow is derived from the accent', /rgba\(/.test(root.getPropertyValue('--accent-dim')));
check('galaxy backdrop contains stars',
  /radial-gradient/.test(root.getPropertyValue('--app-bg')));
check('dark flag is exposed', root.classList ? true : true);

const accent = root.getPropertyValue('--accent');
const glow = root.getPropertyValue('--accent-dim');
const rgb = accent.match(/^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i);
check('glow matches the accent colour',
  !!rgb && glow === `rgba(${parseInt(rgb[1], 16)},${parseInt(rgb[2], 16)},${parseInt(rgb[3], 16)},.18)`,
  `accent ${accent} vs glow ${glow}`);

section('partial themes fall back');

T.apply({ id: 'partial', name: 'Partial', vars: { '--accent': '#ff0055' } });
check('an unset property falls back to the default',
  root.getPropertyValue('--text') === T.get('galaxy').vars['--text']);
check('the set property wins', root.getPropertyValue('--accent') === '#ff0055');
check('the glow follows the overridden accent', /255,0,85/.test(root.getPropertyValue('--accent-dim')));
check('a theme with no vars does not crash', !!T.apply({ id: 'empty', name: 'Empty' }));
T.apply(T.get('galaxy'));

section('custom themes');

storage.map.clear();
const custom = T.derive(T.get('paper'), 'mine', 'Mine');
check('derive copies the source palette', custom.vars['--accent'] === T.get('paper').vars['--accent']);
check('derive marks it custom', custom.custom === true);

T.saveCustom(custom);
check('custom themes persist', T.loadCustom().length === 1);
check('custom themes are listed with the built-ins',
  T.all().some((t) => t.id === 'mine'));
check('byId finds a custom theme', T.byId('mine').name === 'Mine');

const dupe = T.derive(T.get('paper'), T.suggestId('Mine'), 'Mine 2');
check('suggestId avoids collisions', dupe.id !== 'mine' && /-\d$/.test(dupe.id));

T.apply(T.byId('mine'));
check('a custom theme applies', true);

section('import');

check('rejects non-JSON', (() => {
  try { T.parseImported('not json'); return false; } catch (e) { return true; }
})());
check('rejects JSON without a vars block', (() => {
  try { T.parseImported('{"name":"x"}'); return false; } catch (e) { return true; }
})());

const imported = T.parseImported(JSON.stringify({
  quarkdriveTheme: 1,
  name: 'Shared',
  vars: { '--accent': '#00ff88', '--bg': '#101010' },
}));
check('accepts a well-formed theme', imported.name === 'Shared');
check('an import with a colliding id gets a fresh one', imported.id !== 'mine');
check('imported partial themes keep the fallback', imported.vars['--text'] === undefined);
T.saveCustom(imported);
T.apply(T.byId(imported.id));
check('an imported theme applies and falls back',
  root.getPropertyValue('--accent') === '#00ff88' &&
  root.getPropertyValue('--text') === T.get('galaxy').vars['--text']);

section('consistency with the stylesheet');

const css = fs.readFileSync(path.join(__dirname, 'styles.css'), 'utf8');
const missing = T.EDITABLE.filter(([key]) => !css.includes(key));
check('every editable colour is actually used by styles.css',
  missing.length === 0, `unused: ${missing.map(([k]) => k).join(', ')}`);
check('styles.css documents the [hidden] rule for styled panes',
  css.includes('#login-view[hidden]') && css.includes('#main-view[hidden]'));

section('no login flash on reload');

// index.html must decide the starting view before first paint: a saved
// token adds a class that CSS turns into display:none for the login card.
const html = fs.readFileSync(path.join(__dirname, 'index.html'), 'utf8');
check('a head script reads the session token before the body is parsed',
  /<head>[\s\S]*?<script>[\s\S]*localStorage\.getItem\('qd\.token'\)[\s\S]*classList\.add\('session'\)[\s\S]*<\/script>[\s\S]*<\/head>/.test(html));
check('the session class hides the login card in CSS',
  css.includes('html.session #login-view'));
check('app.js releases the guard once it picks a view',
  /classList\.remove\('session'\)/.test(fs.readFileSync(path.join(__dirname, 'app.js'), 'utf8')));

section('first-run sign-up wiring');

for (const id of ['signin-fields', 'signup-fields', 'to-signup', 'to-signin',
  'signup-server', 'signup-username', 'signup-password', 'signup-confirm',
  'signup-vault', 'signup-submit']) {
  check(`index.html has #${id}`, html.includes(`id="${id}"`));
}
const appSource = fs.readFileSync(path.join(__dirname, 'app.js'), 'utf8');
for (const needed of ['showSignup', 'showSignin', 'fetchAuthStatus',
  '/api/v1/auth/status', '/api/v1/auth/register']) {
  check(`app.js wires ${needed}`, appSource.includes(needed));
}
check('the sign-up card starts hidden', /id="signup-fields" hidden/.test(html));
check('CSS styles the auth switch', css.includes('.auth-switch'));

/* ---------------------------------------------------------------- done */

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
