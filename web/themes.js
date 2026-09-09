/* Quarkdrive themes.
 *
 * A theme is nothing but a set of CSS custom properties. Everything in
 * styles.css reads from those properties, so applying a theme is just writing
 * them onto :root — no rebuild, no second stylesheet.
 *
 * Built-in themes live here. Themes a user creates are stored in
 * localStorage by app.js and take exactly the same shape, which is also the
 * shape of an exported theme file.
 *
 * Loaded before app.js; exposes window.QuarkdriveThemes.
 */

'use strict';

(function () {
  /** The properties a theme may set, in editor order. */
  const EDITABLE = [
    ['--bg', 'Page background'],
    ['--bg-elev', 'Panels & cards'],
    ['--bg-hover', 'Hovered rows'],
    ['--border', 'Borders'],
    ['--text', 'Text'],
    ['--text-dim', 'Secondary text'],
    ['--accent', 'Accent'],
    ['--accent-contrast', 'Text on accent'],
    ['--danger', 'Danger'],
    ['--ok', 'Success'],
  ];

  /** Deterministic PRNG so a starfield looks the same on every load. */
  function rng(seed) {
    let s = seed | 0 || 1;
    return function () {
      s ^= s << 13; s |= 0;
      s ^= s >>> 7;
      s ^= s << 17; s |= 0;
      return ((s >>> 0) % 10000) / 10000;
    };
  }

  /** A field of tiny stars, as radial-gradient layers. */
  function starfield(seed, count, colour) {
    const next = rng(seed);
    const stars = [];
    for (let i = 0; i < count; i++) {
      const x = (next() * 100).toFixed(2);
      const y = (next() * 100).toFixed(2);
      const r = (0.5 + next() * 1.5).toFixed(2);
      const a = (0.25 + next() * 0.7).toFixed(2);
      stars.push(`radial-gradient(${r}px ${r}px at ${x}% ${y}%, rgba(${colour},${a}) 42%, transparent 46%)`);
    }
    return stars;
  }

  /** Two soft colour washes plus a base gradient, for a nebula backdrop. */
  function sky(wash1, wash2, base) {
    return [
      `radial-gradient(ellipse 90% 55% at 72% -12%, ${wash1}, transparent 62%)`,
      `radial-gradient(ellipse 80% 50% at 12% 112%, ${wash2}, transparent 58%)`,
      base,
    ];
  }

  function galaxyBackdrop() {
    return [
      ...starfield(0x51edce66, 90, '255,255,255'),
      ...sky(
        'rgba(139,92,246,.42)',
        'rgba(76,29,149,.45)',
        'linear-gradient(165deg, #07061160 0%, transparent 100%)',
      ),
      'linear-gradient(160deg, #0a0918 0%, #130f28 45%, #1a1033 100%)',
    ].join(',');
  }

  function nebulaBackdrop() {
    return [
      ...starfield(0x2f9ab7e1, 70, '255,235,250'),
      ...sky(
        'rgba(236,72,153,.35)',
        'rgba(129,60,220,.38)',
        'linear-gradient(165deg, #12071a60 0%, transparent 100%)',
      ),
      'linear-gradient(160deg, #160a1e 0%, #241033 50%, #2b0f2e 100%)',
    ].join(',');
  }

  const THEMES = [
    {
      id: 'galaxy',
      name: 'Galaxy',
      dark: true,
      blurb: 'Deep violet, drifting stars. The default.',
      swatch: ['#0a0918', '#a78bfa', '#2e2654'],
      vars: {
        '--bg': '#0a0918',
        '--bg-elev': '#171330',
        '--bg-hover': '#241d44',
        '--border': '#322a55',
        '--text': '#ece9ff',
        '--text-dim': '#a79fd0',
        '--accent': '#a78bfa',
        '--accent-hover': '#c4b5fd',
        '--accent-dim': 'rgba(167,139,250,.18)',
        '--accent-contrast': '#170f2b',
        '--danger': '#fb7185',
        '--ok': '#34d399',
        '--panel-opacity': '78%',
        '--panel-blur': '16px',
        '--app-bg': galaxyBackdrop(),
      },
    },
    {
      id: 'nebula',
      name: 'Nebula',
      dark: true,
      blurb: 'Magenta clouds over a darker violet.',
      swatch: ['#160a1e', '#f472b6', '#3b1d4d'],
      vars: {
        '--bg': '#160a1e',
        '--bg-elev': '#241033',
        '--bg-hover': '#35184a',
        '--border': '#4a2358',
        '--text': '#fdeef7',
        '--text-dim': '#cf9fc4',
        '--accent': '#f472b6',
        '--accent-hover': '#f9a8d4',
        '--accent-dim': 'rgba(244,114,182,.18)',
        '--accent-contrast': '#2b0a1c',
        '--danger': '#f87171',
        '--ok': '#4ade80',
        '--panel-opacity': '76%',
        '--panel-blur': '16px',
        '--app-bg': nebulaBackdrop(),
      },
    },
    {
      id: 'midnight',
      name: 'Midnight',
      dark: true,
      blurb: 'Near-black blue, quiet and high contrast.',
      swatch: ['#0b0f14', '#60a5fa', '#1b2430'],
      vars: {
        '--bg': '#0b0f14',
        '--bg-elev': '#141a22',
        '--bg-hover': '#1d2630',
        '--border': '#26303c',
        '--text': '#e6edf3',
        '--text-dim': '#94a3b8',
        '--accent': '#60a5fa',
        '--accent-hover': '#93c5fd',
        '--accent-dim': 'rgba(96,165,250,.16)',
        '--accent-contrast': '#0b1220',
        '--danger': '#f87171',
        '--ok': '#34d399',
        '--panel-opacity': '100%',
        '--panel-blur': '0px',
        '--app-bg': 'linear-gradient(180deg, #0b0f14 0%, #0d1219 100%)',
      },
    },
    {
      id: 'aurora',
      name: 'Aurora',
      dark: true,
      blurb: 'Cold teal with a green wash.',
      swatch: ['#071a18', '#2dd4bf', '#0f2f2a'],
      vars: {
        '--bg': '#071a18',
        '--bg-elev': '#0d2622',
        '--bg-hover': '#12332e',
        '--border': '#1b4038',
        '--text': '#e6fff8',
        '--text-dim': '#8fc4b8',
        '--accent': '#2dd4bf',
        '--accent-hover': '#5eead4',
        '--accent-dim': 'rgba(45,212,191,.16)',
        '--accent-contrast': '#04211c',
        '--danger': '#fb7185',
        '--ok': '#a3e635',
        '--panel-opacity': '82%',
        '--panel-blur': '12px',
        '--app-bg': [
          ...starfield(0x77c1aa31, 30, '180,255,240'),
          ...sky('rgba(45,212,191,.24)', 'rgba(16,185,129,.20)',
            'linear-gradient(160deg, #071a18 0%, #0b221f 100%)'),
        ].join(','),
      },
    },
    {
      id: 'ember',
      name: 'Ember',
      dark: true,
      blurb: 'Warm charcoal, amber accent.',
      swatch: ['#171210', '#fb923c', '#33231b'],
      vars: {
        '--bg': '#171210',
        '--bg-elev': '#221a15',
        '--bg-hover': '#2e231b',
        '--border': '#3c2c21',
        '--text': '#f7ede4',
        '--text-dim': '#c0a896',
        '--accent': '#fb923c',
        '--accent-hover': '#fdba74',
        '--accent-dim': 'rgba(251,146,60,.16)',
        '--accent-contrast': '#2a1204',
        '--danger': '#ef4444',
        '--ok': '#84cc16',
        '--panel-opacity': '100%',
        '--panel-blur': '0px',
        '--app-bg': 'linear-gradient(165deg, #1a1310 0%, #17110e 60%, #1f120c 100%)',
      },
    },
    {
      id: 'paper',
      name: 'Paper',
      dark: false,
      blurb: 'Plain light theme, easy on the eyes in daylight.',
      swatch: ['#f7f8fa', '#2563eb', '#e6e9ef'],
      vars: {
        '--bg': '#f7f8fa',
        '--bg-elev': '#ffffff',
        '--bg-hover': '#eef1f5',
        '--border': '#dde3ea',
        '--text': '#1b2430',
        '--text-dim': '#5c6b7f',
        '--accent': '#2563eb',
        '--accent-hover': '#1d4ed8',
        '--accent-dim': 'rgba(37,99,235,.10)',
        '--accent-contrast': '#ffffff',
        '--danger': '#dc2626',
        '--ok': '#15803d',
        '--panel-opacity': '100%',
        '--panel-blur': '0px',
        '--app-bg': 'linear-gradient(180deg, #f7f8fa 0%, #eef1f6 100%)',
      },
    },
    {
      id: 'linen',
      name: 'Linen',
      dark: false,
      blurb: 'Warm light theme, low glare.',
      swatch: ['#f6f1ea', '#b45309', '#e8dfd2'],
      vars: {
        '--bg': '#f6f1ea',
        '--bg-elev': '#fffdf9',
        '--bg-hover': '#efe7db',
        '--border': '#e0d5c5',
        '--text': '#33291f',
        '--text-dim': '#7a6a58',
        '--accent': '#b45309',
        '--accent-hover': '#92400e',
        '--accent-dim': 'rgba(180,83,9,.12)',
        '--accent-contrast': '#fffdf9',
        '--danger': '#b91c1c',
        '--ok': '#4d7c0f',
        '--panel-opacity': '100%',
        '--panel-blur': '0px',
        '--app-bg': 'linear-gradient(180deg, #f8f3ec 0%, #efe8dd 100%)',
      },
    },
  ];

  /** Themes a user has made, kept by app.js in localStorage. */
  function loadCustom() {
    try {
      const raw = JSON.parse(localStorage.getItem('qd.customThemes') || '[]');
      return Array.isArray(raw) ? raw.filter((t) => t && t.id && t.vars) : [];
    } catch (_) {
      return [];
    }
  }

  /**
   * Store custom themes, replacing whatever was there.
   *
   * Accepts a single theme or a list, since both are easy to pass by mistake
   * and only one of them survives a round trip otherwise.
   */
  function saveCustom(themes) {
    const list = Array.isArray(themes) ? themes : [themes];
    localStorage.setItem('qd.customThemes', JSON.stringify(list));
  }

  /** Every theme, built-ins first, then the user's own. */
  function all() {
    return THEMES.concat(loadCustom());
  }

  function byId(id) {
    return all().find((t) => t.id === id) || null;
  }

  function get(id) {
    return byId(id) || byId('galaxy');
  }

  /** Default values, used when a theme omits a property. */
  function fallbacks() {
    return byId('galaxy').vars;
  }

  /**
   * Apply a theme by writing its properties onto :root.
   *
   * Unset properties fall back to the built-in defaults, so a hand-made theme
   * can be as partial as the user likes — change one colour and you have a
   * theme.
   */
  function apply(theme) {
    const root = document.documentElement;
    const vars = Object.assign({}, fallbacks(), theme && theme.vars ? theme.vars : {});
    for (const [key, value] of Object.entries(deriveAccent(vars))) {
      root.style.setProperty(key, value);
    }
    root.classList.toggle('theme-dark', theme ? theme.dark !== false : true);
    return theme;
  }

  /**
   * Derive the translucent accent glow from the accent colour.
   *
   * It is not offered in the editor because a colour picker cannot express
   * alpha, and the only sensible value for it is "the accent, but faint".
   */
  function deriveAccent(vars) {
    const out = Object.assign({}, vars);
    const hex = out['--accent'];
    if (typeof hex === 'string' && /^#[0-9a-f]{6}$/i.test(hex)) {
      const n = parseInt(hex.slice(1), 16);
      out['--accent-dim'] =
        `rgba(${(n >> 16) & 255},${(n >> 8) & 255},${n & 255},.18)`;
    }
    return out;
  }

  /** Reset to the built-in defaults, e.g. before previewing a draft. */
  function reset() {
    apply(get('galaxy'));
  }

  /** A fresh, editable copy of an existing theme. */
  function derive(source, newId, newName) {
    return {
      id: newId,
      name: newName,
      dark: source.dark !== false,
      custom: true,
      blurb: 'Your theme.',
      swatch: (source.swatch || ['#000000', '#ffffff', '#222222']).slice(0, 3),
      vars: Object.assign({}, source.vars),
    };
  }

  /** Read an exported theme file. Throws if it is not something we can use. */
  function parseImported(text) {
    let data;
    try {
      data = JSON.parse(text);
    } catch (_) {
      throw new Error('that is not valid JSON');
    }
    const theme = Array.isArray(data) ? data[0] : data;
    if (!theme || typeof theme !== 'object' || !theme.vars) {
      throw new Error('missing a "vars" block — is this a Quarkdrive theme?');
    }
    const base = derive(theme, suggestId(theme.name || 'imported'), theme.name || 'Imported theme');
    base.blurb = theme.blurb || 'Imported theme.';
    base.dark = theme.dark !== false;
    base.swatch = theme.swatch || base.swatch;
    return base;
  }

  function suggestId(name) {
    const clean = String(name).toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');
    let id = clean || 'theme';
    let n = 2;
    while (byId(id)) id = `${clean}-${n++}`;
    return id;
  }

  window.QuarkdriveThemes = {
    EDITABLE,
    all,
    get,
    byId,
    apply,
    reset,
    derive,
    loadCustom,
    saveCustom,
    parseImported,
    suggestId,
    DEFAULT_ID: 'galaxy',
  };
})();
