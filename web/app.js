/* Quarkdrive web UI.
 *
 * Plain ES2020 against the server's /api/v1 endpoints. No framework, no
 * bundler, no npm: this file is served exactly as written.
 *
 * Three things worth knowing before editing:
 *
 * 1. Every API call needs an Authorization header, which an <img src> cannot
 *    send. Thumbnails and full-size photos are therefore fetched as
 *    authenticated blobs and turned into object URLs, loaded lazily.
 * 2. Any element styled with a `display` property needs an explicit
 *    `[hidden] { display: none }` rule in styles.css, or the hidden attribute
 *    is silently ignored. This bit once: the login screen would not go away.
 * 3. Themes are CSS custom properties (see themes.js). Nothing here should
 *    hard-code a colour.
 */

'use strict';

const $ = (sel) => document.querySelector(sel);

const STORAGE = {
  server: 'qd.server',
  token: 'qd.token',
  vault: 'qd.vault',
  theme: 'qd.theme',
  custom: 'qd.customThemes',
  bgPrefix: 'qd.bgimage.',
};

const state = {
  server: localStorage.getItem(STORAGE.server) || '',
  token: localStorage.getItem(STORAGE.token) || '',
  vault: localStorage.getItem(STORAGE.vault) || '',
  path: '',
  filter: '',
  entries: [],
  photos: [],
  lightbox: -1,
};

/* ------------------------------------------------------------- helpers */

const apiUrl = (pathname) => state.server.replace(/\/+$/, '') + pathname;
const vaultApi = (suffix) => `/api/v1/vaults/${encodeURIComponent(state.vault)}${suffix}`;

async function api(pathname, options = {}) {
  const res = await fetch(apiUrl(pathname), {
    ...options,
    headers: { ...(options.headers || {}), Authorization: `Bearer ${state.token}` },
  });
  if (res.status === 401) {
    signOut();
    throw new Error('Your session expired — please sign in again.');
  }
  if (!res.ok) {
    let message = `${res.status} ${res.statusText}`;
    try {
      const body = await res.json();
      if (body && body.error) message = body.error;
    } catch (_) {
      /* non-JSON error body; keep the status text */
    }
    throw new Error(message);
  }
  return res;
}

function formatBytes(n) {
  if (!n) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  const i = Math.min(Math.floor(Math.log(n) / Math.log(1024)), units.length - 1);
  const value = n / Math.pow(1024, i);
  return `${value.toFixed(value < 10 && i > 0 ? 1 : 0)} ${units[i]}`;
}

function formatDate(ts) {
  if (!ts) return '';
  return new Date(ts * 1000).toLocaleString(undefined, {
    year: 'numeric', month: 'short', day: 'numeric',
    hour: '2-digit', minute: '2-digit',
  });
}

function dayKey(ts) {
  const d = ts ? new Date(ts * 1000) : new Date(0);
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
}

function iconFor(entry) {
  if (entry.kind === 'dir') return '📁';
  if (entry.kind === 'symlink') return '🔗';
  if (entry.mime && entry.mime.startsWith('image/')) return '🖼️';
  if (entry.mime && entry.mime.startsWith('video/')) return '🎬';
  if (entry.mime && entry.mime.startsWith('audio/')) return '🎵';
  if (/\.(zip|gz|tgz|bz2|xz)$/i.test(entry.name)) return '🗜️';
  if (/\.(pdf|docx?|txt|md|rtf)$/i.test(entry.name)) return '📄';
  return '📦';
}

function toast(message, kind = '') {
  const el = document.createElement('div');
  el.className = `toast ${kind}`.trim();
  el.textContent = message;
  $('#toasts').appendChild(el);
  setTimeout(() => el.remove(), kind === 'error' ? 8000 : 3500);
}

function setStatus(text) {
  $('#statusbar').textContent = text;
}

/* --------------------------------------------------------------- auth */

async function signIn(server, username, password) {
  // Deliberately not via api(): there is no token yet.
  const res = await fetch(`${server.replace(/\/+$/, '')}/api/v1/auth/login`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ username, password }),
  });
  if (!res.ok) {
    let message = `${res.status} ${res.statusText}`;
    try {
      const body = await res.json();
      if (body && body.error) message = body.error;
    } catch (_) { /* ignore */ }
    throw new Error(message);
  }
  const data = await res.json();
  state.server = server.replace(/\/+$/, '');
  state.token = data.token;
  localStorage.setItem(STORAGE.server, state.server);
  localStorage.setItem(STORAGE.token, state.token);
}

function signOut() {
  state.token = '';
  state.vault = '';
  state.photos = [];
  state.entries = [];
  localStorage.removeItem(STORAGE.token);
  showLogin();
}

function showLogin() {
  $('#login-view').hidden = false;
  $('#main-view').hidden = true;
  // We know which view to show now; release the pre-paint guard from index.html.
  document.documentElement.classList.remove('session');
  if (!state.server) $('#login-server').value = window.location.origin;
  $('#login-error').hidden = true;
  showSignin();
  // A server waiting for its first account opens on the sign-up card.
  fetchAuthStatus().then((status) => {
    if (status.first_run && !$('#login-view').hidden) showSignup();
  });
}

function showApp() {
  $('#login-view').hidden = true;
  $('#main-view').hidden = false;
  document.documentElement.classList.remove('session');
}

/* First-run sign-up: a server with no accounts offers to create its first
 * one, Gitea/Nextcloud-style. Once an account exists the server closes
 * registration and these fields stay out of the way. */

function showSignup() {
  $('#signin-fields').hidden = true;
  $('#signup-fields').hidden = false;
  $('#signup-server').value = $('#login-server').value.trim() || window.location.origin;
  $('#signup-vault').placeholder = $('#signup-username').value.trim() || 'photos';
  $('#login-error').hidden = true;
}

function showSignin() {
  $('#signup-fields').hidden = true;
  $('#signin-fields').hidden = false;
  $('#login-error').hidden = true;
}

/** Ask the server whether it is still waiting for its first account. */
function fetchAuthStatus() {
  const server = $('#login-server').value.trim() || window.location.origin;
  return fetch(`${server.replace(/\/+$/, '')}/api/v1/auth/status`)
    .then((res) => (res.ok ? res.json() : { first_run: false }))
    .catch(() => ({ first_run: false }));
}

/** Create the first account on a fresh server and walk straight in. */
async function register(username, password, vault, server) {
  const res = await fetch(`${server.replace(/\/+$/, '')}/api/v1/auth/register`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ username, password, vault }),
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) {
    throw new Error(data.error || `${res.status} ${res.statusText}`);
  }
  state.server = server.replace(/\/+$/, '');
  state.token = data.token;
  state.vault = data.vault || vault || username;
  localStorage.setItem(STORAGE.server, state.server);
  localStorage.setItem(STORAGE.token, state.token);
  localStorage.setItem(STORAGE.vault, state.vault);
}

/* ------------------------------------------------------------- vaults */

async function loadVaults() {
  const data = await (await api('/api/v1/vaults')).json();
  const select = $('#vault-select');
  select.innerHTML = '';

  if (!data.vaults.length) {
    select.hidden = true;
    state.vault = '';
    throw new Error('This account has no vaults yet. Create one with `quarkdrive-server create-vault`.');
  }

  select.hidden = false;
  for (const v of data.vaults) {
    const opt = document.createElement('option');
    opt.value = v.name;
    opt.textContent = v.encrypted ? `${v.name} (encrypted)` : v.name;
    select.appendChild(opt);
  }
  if (!data.vaults.some((v) => v.name === state.vault)) {
    state.vault = data.vaults[0].name;
    localStorage.setItem(STORAGE.vault, state.vault);
  }
  select.value = state.vault;
}

/* -------------------------------------------------------------- files */

async function loadFiles() {
  const query = state.path ? `?path=${encodeURIComponent(state.path)}` : '';
  const data = await (await api(vaultApi('/fs' + query))).json();
  state.entries = data.entries;
  renderFiles();
}

function renderFiles() {
  renderBreadcrumb();

  const list = $('#entry-list');
  list.innerHTML = '';

  const needle = state.filter.trim().toLowerCase();
  const visible = needle
    ? state.entries.filter((e) => e.name.toLowerCase().includes(needle))
    : state.entries;

  if (!visible.length) {
    const li = document.createElement('li');
    li.className = 'empty';
    if (needle) {
      li.innerHTML = '<span class="empty-icon">🔍</span>';
      li.appendChild(document.createTextNode(`No matches for “${needle}” in this folder.`));
    } else {
      li.innerHTML = '<span class="empty-icon">🗃️</span>' +
        'Nothing here yet.<br>Upload a file, or drop one onto this area.';
    }
    list.appendChild(li);
    setStatus(needle ? `No matches for “${needle}”` : 'Empty folder');
    return;
  }

  for (const entry of visible) {
    const li = document.createElement('li');
    li.className = 'entry';

    const icon = document.createElement('span');
    icon.className = 'entry-icon';
    icon.textContent = iconFor(entry);

    const name = document.createElement('button');
    name.className = 'entry-name';
    name.textContent = entry.name;
    name.title = entry.name;
    name.addEventListener('click', () => {
      if (entry.kind === 'dir') navigateTo(entry.path);
      else openFile(entry);
    });

    const meta = document.createElement('span');
    meta.className = 'entry-meta';
    meta.textContent = entry.kind === 'dir'
      ? 'Folder'
      : `${formatBytes(entry.size)} · ${formatDate(entry.mtime)}`;

    const actions = document.createElement('span');
    actions.className = 'entry-actions';

    const ren = document.createElement('button');
    ren.className = 'ghost';
    ren.textContent = 'Rename';
    ren.addEventListener('click', () => renameEntry(entry));
    actions.appendChild(ren);

    if (entry.kind !== 'dir') {
      const dl = document.createElement('button');
      dl.className = 'ghost';
      dl.textContent = 'Download';
      dl.addEventListener('click', () => downloadFile(entry.path));
      actions.appendChild(dl);
    }

    const del = document.createElement('button');
    del.className = 'ghost danger';
    del.textContent = 'Delete';
    del.addEventListener('click', () => removeEntry(entry));
    actions.appendChild(del);

    li.append(icon, name, meta, actions);
    list.appendChild(li);
  }

  const files = visible.filter((e) => e.kind !== 'dir').length;
  const dirs = visible.length - files;
  const bytes = visible.reduce((sum, e) => sum + (e.size || 0), 0);
  const shown = needle ? ` (${visible.length} of ${state.entries.length} shown)` : '';
  setStatus(`${state.path || '/'} — ${dirs} folder(s), ${files} file(s), ` +
    `${formatBytes(bytes)}${shown}`);
}

function renderBreadcrumb() {
  const crumbs = $('#breadcrumb');
  crumbs.innerHTML = '';

  const root = document.createElement('button');
  root.textContent = state.vault;
  root.addEventListener('click', () => navigateTo(''));
  crumbs.appendChild(root);

  if (state.path) {
    let acc = '';
    for (const part of state.path.split('/').filter(Boolean)) {
      const sep = document.createElement('span');
      sep.className = 'sep';
      sep.textContent = '/';
      crumbs.appendChild(sep);
      acc = acc ? `${acc}/${part}` : part;

      if (acc === state.path) {
        const cur = document.createElement('span');
        cur.className = 'current';
        cur.textContent = part;
        crumbs.appendChild(cur);
      } else {
        const btn = document.createElement('button');
        btn.textContent = part;
        const target = acc;
        btn.addEventListener('click', () => navigateTo(target));
        crumbs.appendChild(btn);
      }
    }
  }
}

function navigateTo(path) {
  state.path = path;
  loadFiles().catch((e) => toast(e.message, 'error'));
}

async function downloadFile(path) {
  try {
    const res = await api(vaultApi(`/fs/download?path=${encodeURIComponent(path)}`));
    const blob = await res.blob();
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = path.split('/').pop() || 'download';
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 20000);
  } catch (e) {
    toast(`Download failed: ${e.message}`, 'error');
  }
}

/** Images open in the lightbox; everything else downloads. */
function openFile(entry) {
  if (entry.mime && entry.mime.startsWith('image/')) {
    const index = state.photos.findIndex((p) => p.path === entry.path);
    if (index >= 0) {
      openLightbox(index);
      return;
    }
  }
  downloadFile(entry.path);
}

async function removeEntry(entry) {
  const what = entry.kind === 'dir' ? `folder "${entry.name}" and everything in it` : entry.name;
  if (!window.confirm(`Delete ${what}? This syncs to every device.`)) return;
  try {
    await api(vaultApi(`/fs?path=${encodeURIComponent(entry.path)}`), { method: 'DELETE' });
    toast(`Deleted ${entry.name}`, 'ok');
    await Promise.all([loadFiles(), loadTimeline()]);
  } catch (e) {
    toast(`Delete failed: ${e.message}`, 'error');
  }
}

async function uploadFiles(fileList) {
  const files = Array.from(fileList);
  if (!files.length) return;
  let done = 0;
  for (const file of files) {
    const rel = state.path ? `${state.path}/${file.name}` : file.name;
    try {
      await api(vaultApi(`/fs?path=${encodeURIComponent(rel)}`), {
        method: 'PUT',
        headers: { 'Content-Type': 'application/octet-stream' },
        body: file,
      });
      done += 1;
      setStatus(`Uploading ${done}/${files.length}…`);
    } catch (e) {
      toast(`Upload of ${file.name} failed: ${e.message}`, 'error');
    }
  }
  if (done) toast(`Uploaded ${done} file${done === 1 ? '' : 's'}`, 'ok');
  await Promise.all([loadFiles(), loadTimeline()]);
}

async function createFolder() {
  const name = window.prompt('Folder name');
  if (!name) return;
  const rel = state.path ? `${state.path}/${name}` : name;
  try {
    await api(vaultApi(`/fs/mkdir?path=${encodeURIComponent(rel)}`), { method: 'POST' });
    toast(`Created ${name}`, 'ok');
    await loadFiles();
  } catch (e) {
    toast(`Could not create folder: ${e.message}`, 'error');
  }
}

/** Rename an entry, or move it by giving a path. */
async function renameEntry(entry) {
  const name = window.prompt(
    'New name — use a path to move it into a folder', entry.name,
  );
  if (!name || name === entry.name) return;

  const trimmed = name.trim().replace(/^\/+/, '');
  const parent = entry.path.includes('/')
    ? entry.path.slice(0, entry.path.lastIndexOf('/')) : '';
  const to = parent ? `${parent}/${trimmed}` : trimmed;

  try {
    await api(vaultApi(`/fs/move?from=${encodeURIComponent(entry.path)}` +
      `&to=${encodeURIComponent(to)}`), { method: 'POST' });
    toast(entry.path === to ? 'Renamed' : `Moved to ${to}`, 'ok');
    await Promise.all([loadFiles(), loadTimeline()]);
  } catch (e) {
    toast(`Rename failed: ${e.message}`, 'error');
  }
}

/** Create a vault and switch to it. */
async function createVault() {
  const name = window.prompt('Name for the new vault');
  if (!name || !name.trim()) return;
  try {
    await api('/api/v1/vaults', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name: name.trim(), encrypted: false }),
    });
    state.vault = name.trim();
    localStorage.setItem(STORAGE.vault, state.vault);
    state.path = '';
    await loadVaults();
    $('#vault-select').value = state.vault;
    await Promise.all([loadFiles(), loadTimeline()]);
    toast(`Created vault “${name.trim()}”`, 'ok');
  } catch (e) {
    toast(`Could not create vault: ${e.message}`, 'error');
  }
}

/* ------------------------------------------------------------- photos */

let objectUrls = [];

function releaseObjectUrls() {
  for (const url of objectUrls) URL.revokeObjectURL(url);
  objectUrls = [];
}

async function loadTimeline() {
  const data = await (await api(vaultApi('/timeline?limit=2000'))).json();
  state.photos = data.items;
  renderTimeline();
}

function renderTimeline() {
  const root = $('#timeline');
  root.innerHTML = '';
  releaseObjectUrls();

  if (!state.photos.length) {
    const p = document.createElement('p');
    p.className = 'empty';
    p.innerHTML = '<span class="empty-icon">🖼️</span>No photos in this vault yet.';
    root.appendChild(p);
    setStatus('No photos');
    return;
  }

  const groups = new Map();
  state.photos.forEach((item, index) => {
    const key = dayKey(item.taken_at);
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push({ item, index });
  });

  for (const [day, items] of groups) {
    const section = document.createElement('section');
    section.className = 'timeline-group';

    const heading = document.createElement('h2');
    heading.className = 'timeline-date';
    heading.textContent = new Date(`${day}T00:00:00`).toLocaleDateString(undefined, {
      weekday: 'long', year: 'numeric', month: 'long', day: 'numeric',
    });

    const grid = document.createElement('div');
    grid.className = 'photo-grid';

    for (const { item, index } of items) {
      const cell = document.createElement('div');
      cell.className = 'photo';
      cell.tabIndex = 0;
      cell.setAttribute('role', 'button');
      cell.title = item.path;

      const img = document.createElement('img');
      img.alt = item.path.split('/').pop() || 'photo';
      img.loading = 'lazy';
      img.dataset.src = apiUrl(item.thumb);
      img.dataset.index = String(index);
      cell.appendChild(img);

      const open = () => openLightbox(index);
      cell.addEventListener('click', open);
      cell.addEventListener('keydown', (ev) => {
        if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); open(); }
      });

      grid.appendChild(cell);
    }

    section.append(heading, grid);
    root.appendChild(section);
  }

  observeThumbnails(root);
  setStatus(`${state.photos.length} photo${state.photos.length === 1 ? '' : 's'}`);
}

let observer = null;

function observeThumbnails(root) {
  if (observer) observer.disconnect();

  if (!('IntersectionObserver' in window)) {
    root.querySelectorAll('img[data-src]').forEach(loadThumbnail);
    return;
  }

  observer = new IntersectionObserver((entries) => {
    for (const entry of entries) {
      if (!entry.isIntersecting) continue;
      observer.unobserve(entry.target);
      loadThumbnail(entry.target);
    }
  }, { rootMargin: '300px' });

  root.querySelectorAll('img[data-src]').forEach((img) => observer.observe(img));
}

async function loadThumbnail(img) {
  const src = img.dataset.src;
  if (!src) return;
  try {
    const res = await fetch(src, { headers: { Authorization: `Bearer ${state.token}` } });
    if (!res.ok) throw new Error(res.statusText);
    const url = URL.createObjectURL(await res.blob());
    objectUrls.push(url);
    img.src = url;
  } catch (_) {
    img.replaceWith(fallbackTile(img.alt));
  }
}

function fallbackTile(label) {
  const div = document.createElement('div');
  div.className = 'photo-fallback';
  div.textContent = label;
  return div;
}

/* ----------------------------------------------------------- lightbox */

let lightboxUrl = null;

async function openLightbox(index) {
  if (index < 0 || index >= state.photos.length) return;
  state.lightbox = index;
  $('#lightbox').hidden = false;
  await showLightboxImage();
}

async function showLightboxImage() {
  const item = state.photos[state.lightbox];
  if (!item) return;

  $('#lb-caption').textContent = `${item.path} · ${formatDate(item.taken_at)}`;
  const img = $('#lb-image');
  img.alt = item.path;
  img.src = '';

  try {
    const res = await api(vaultApi(`/fs/download?path=${encodeURIComponent(item.path)}`));
    if (lightboxUrl) URL.revokeObjectURL(lightboxUrl);
    lightboxUrl = URL.createObjectURL(await res.blob());
    img.src = lightboxUrl;
  } catch (e) {
    $('#lb-caption').textContent = `Could not load ${item.path}: ${e.message}`;
  }
}

function closeLightbox() {
  $('#lightbox').hidden = true;
  state.lightbox = -1;
  if (lightboxUrl) { URL.revokeObjectURL(lightboxUrl); lightboxUrl = null; }
}

function stepLightbox(delta) {
  if (state.lightbox < 0) return;
  const next = state.lightbox + delta;
  if (next < 0 || next >= state.photos.length) return;
  state.lightbox = next;
  showLightboxImage();
}

/* ================================================================ themes
 *
 * A theme is a bag of CSS custom properties. Applying one writes those
 * properties onto :root; the user's choice and any themes they have made are
 * kept in localStorage.
 */

const Themes = {
  editing: null,   // the draft being edited, or null
  editingWasNew: false,

  currentId() {
    return localStorage.getItem(STORAGE.theme) || QuarkdriveThemes.DEFAULT_ID;
  },

  setCurrentId(id) {
    localStorage.setItem(STORAGE.theme, id);
  },

  /** Apply the theme with `id`, plus any background image saved against it. */
  apply(id) {
    const theme = QuarkdriveThemes.get(id);
    const vars = Object.assign({}, theme.vars);
    const image = localStorage.getItem(STORAGE.bgPrefix + theme.id);
    if (image) vars['--app-bg-image'] = `url("${image}")`;

    const root = document.documentElement;
    for (const [key, value] of Object.entries(vars)) {
      root.style.setProperty(key, value);
    }
    // A background is per theme: switching must clear the previous one.
    if (!vars['--app-bg-image']) root.style.removeProperty('--app-bg-image');

    root.classList.toggle('theme-dark', theme.dark !== false);
    this.setCurrentId(theme.id);
    return theme;
  },

  applyCurrent() {
    return this.apply(this.currentId());
  },

  saveCustom(theme) {
    const all = QuarkdriveThemes.loadCustom().filter((t) => t.id !== theme.id);
    all.push(theme);
    QuarkdriveThemes.saveCustom(all);
  },

  deleteCustom(id) {
    QuarkdriveThemes.saveCustom(QuarkdriveThemes.loadCustom().filter((t) => t.id !== id));
    localStorage.removeItem(STORAGE.bgPrefix + id);
  },

  /** Draft applied live while the editor is open. */
  preview(draft) {
    const vars = Object.assign({}, draft.vars);
    if (draft.backgroundImage) vars['--app-bg-image'] = `url("${draft.backgroundImage}")`;
    const root = document.documentElement;
    for (const [key, value] of Object.entries(vars)) root.style.setProperty(key, value);
    if (!vars['--app-bg-image']) root.style.removeProperty('--app-bg-image');
  },
};

function openThemeDrawer() {
  renderThemeList();
  $('#theme-drawer').hidden = false;
  $('#theme-scrim').hidden = false;
}

function closeThemeDrawer() {
  $('#theme-drawer').hidden = true;
  $('#theme-scrim').hidden = true;
}

function renderThemeList() {
  const host = $('#theme-list');
  const currentId = Themes.currentId();
  host.innerHTML = '';

  for (const theme of QuarkdriveThemes.all()) {
    const card = document.createElement('button');
    card.type = 'button';
    card.className = 'theme-card' + (theme.id === currentId ? ' current' : '');

    const swatch = document.createElement('span');
    swatch.className = 'swatch';
    for (const colour of theme.swatch || []) {
      const bar = document.createElement('span');
      bar.style.background = colour;
      swatch.appendChild(bar);
    }

    const info = document.createElement('span');
    info.className = 'theme-info';
    const name = document.createElement('div');
    name.className = 'theme-name';
    name.textContent = theme.name;
    if (theme.custom || !QuarkdriveThemes.byId(theme.id)) {
      const badge = document.createElement('span');
      badge.className = 'badge';
      badge.textContent = 'custom';
      name.appendChild(badge);
    }
    const blurb = document.createElement('div');
    blurb.className = 'theme-blurb';
    blurb.textContent = theme.blurb || '';
    info.append(name, blurb);

    card.append(swatch, info);
    card.addEventListener('click', () => {
      Themes.apply(theme.id);
      renderThemeList();
      toast(`Theme: ${theme.name}`, 'ok');
    });
    host.appendChild(card);
  }
}

/* ------------------------------------------------------- theme editor */

function openThemeEditor(source) {
  const id = QuarkdriveThemes.suggestId(source ? source.name : 'my-theme');
  const draft = QuarkdriveThemes.derive(source || QuarkdriveThemes.get('galaxy'), id, 'My theme');
  draft.backgroundImage = '';

  Themes.editing = draft;
  Themes.editingWasNew = !source;

  $('#te-title').textContent = source ? `Edit ${source.name}` : 'New theme';
  $('#te-name').value = source ? `${source.name} (copy)` : 'My theme';
  $('#te-error').hidden = true;
  $('#te-delete').hidden = !(source && isCustom(source.id));

  // The "start from" list doubles as the base for a new theme.
  const base = $('#te-base');
  base.innerHTML = '';
  for (const t of QuarkdriveThemes.all()) {
    const opt = document.createElement('option');
    opt.value = t.id;
    opt.textContent = t.name;
    base.appendChild(opt);
  }
  base.value = source ? source.id : QuarkdriveThemes.DEFAULT_ID;
  base.disabled = !source;

  buildColourFields(draft.vars);
  $('#te-bg-url').value = '';
  $('#te-bg-preview').style.setProperty('--app-bg-image', 'none');
  $('#theme-editor').hidden = false;
}

function isCustom(id) {
  return QuarkdriveThemes.loadCustom().some((t) => t.id === id);
}

/** One colour picker plus a hex box per editable property. */
function buildColourFields(vars) {
  const host = $('#te-colours');
  host.innerHTML = '';
  for (const [key, label] of QuarkdriveThemes.EDITABLE) {
    const value = toHex(vars[key]) || '#000000';

    const wrap = document.createElement('div');
    wrap.className = 'colour-field';

    const lab = document.createElement('label');
    lab.textContent = label;
    lab.title = key;

    const picker = document.createElement('input');
    picker.type = 'color';
    picker.value = value;

    const text = document.createElement('input');
    text.type = 'text';
    text.value = value;
    text.spellcheck = false;

    const push = (hex) => {
      const clean = toHex(hex);
      if (!clean) return;
      picker.value = clean;
      text.value = clean;
      Themes.editing.vars[key] = clean;
      Themes.preview(Themes.editing);
      refreshPreview();
    };
    picker.addEventListener('input', () => push(picker.value));
    text.addEventListener('change', () => push(text.value));

    wrap.append(lab, picker, text);
    host.appendChild(wrap);
  }
  refreshPreview();
}

/** Colour pickers only speak hex; anything else falls back to a solid colour. */
function toHex(value) {
  if (typeof value !== 'string') return null;
  const v = value.trim();
  if (/^#[0-9a-f]{6}$/i.test(v)) return v.toLowerCase();
  if (/^#[0-9a-f]{3}$/i.test(v)) {
    return '#' + v.slice(1).split('').map((c) => c + c).join('');
  }
  const m = v.match(/^rgba?\(([^)]+)\)$/i);
  if (m) {
    const parts = m[1].split(',').map((s) => parseFloat(s));
    if (parts.length >= 3 && parts.every((n) => !Number.isNaN(n))) {
      return '#' + parts.slice(0, 3)
        .map((n) => Math.max(0, Math.min(255, Math.round(n))).toString(16).padStart(2, '0'))
        .join('');
    }
  }
  return null;
}

/** Keep the editor's backdrop thumbnail in step with the draft. */
function refreshPreview() {
  const draft = Themes.editing;
  if (!draft) return;
  const el = $('#te-bg-preview');
  el.style.setProperty('--app-bg', draft.vars['--app-bg'] || 'none');
  if (draft.backgroundImage) {
    el.style.setProperty('--app-bg-image', `url("${draft.backgroundImage}")`);
  } else {
    el.style.setProperty('--app-bg-image', 'none');
  }
}

function closeThemeEditor() {
  $('#theme-editor').hidden = true;
  Themes.editing = null;
  Themes.applyCurrent();   // discard the draft's live preview
  renderThemeList();
}

function saveThemeEditor() {
  const draft = Themes.editing;
  if (!draft) return;
  const error = $('#te-error');
  error.hidden = true;

  const name = $('#te-name').value.trim();
  if (!name) {
    error.textContent = 'Give the theme a name.';
    error.hidden = false;
    return;
  }
  draft.name = name;

  if (!Themes.editingWasNew && isCustom(draft.id)) {
    // Editing an existing custom theme: keep its id and background.
    const existing = QuarkdriveThemes.loadCustom().find((t) => t.id === draft.id);
    draft.backgroundImage = draft.backgroundImage
      || (existing && existing.backgroundImage) || '';
  }

  Themes.saveCustom(draft);
  if (draft.backgroundImage) {
    localStorage.setItem(STORAGE.bgPrefix + draft.id, draft.backgroundImage);
  } else {
    localStorage.removeItem(STORAGE.bgPrefix + draft.id);
  }
  Themes.apply(draft.id);
  closeThemeDrawer();
  $('#theme-editor').hidden = true;
  Themes.editing = null;
  toast(`Saved theme “${draft.name}”`, 'ok');
}

function deleteThemeEditor() {
  const draft = Themes.editing;
  if (!draft || !isCustom(draft.id)) return;
  if (!window.confirm(`Delete the theme “${draft.name}”?`)) return;
  Themes.deleteCustom(draft.id);
  $('#theme-editor').hidden = true;
  Themes.editing = null;
  Themes.applyCurrent();
  renderThemeList();
  toast('Theme deleted', 'ok');
}

/** Shrink an uploaded picture so it fits comfortably in localStorage. */
async function sizedDataUrl(file, maxSide = 1920) {
  const raw = await new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result);
    reader.onerror = () => reject(new Error('could not read that file'));
    reader.readAsDataURL(file);
  });
  try {
    const bitmap = await createImageBitmap(file);
    const scale = Math.min(1, maxSide / Math.max(bitmap.width, bitmap.height));
    if (scale < 1 || raw.length > 1_500_000) {
      const canvas = document.createElement('canvas');
      canvas.width = Math.max(1, Math.round(bitmap.width * scale));
      canvas.height = Math.max(1, Math.round(bitmap.height * scale));
      canvas.getContext('2d').drawImage(bitmap, 0, 0, canvas.width, canvas.height);
      return canvas.toDataURL('image/jpeg', 0.82);
    }
    return raw;
  } catch (_) {
    return raw;   // Not a decodable image; let the browser deal with it.
  }
}

function exportTheme(theme) {
  const payload = {
    quarkdriveTheme: 1,
    id: theme.id,
    name: theme.name,
    blurb: theme.blurb,
    dark: theme.dark,
    swatch: theme.swatch,
    vars: theme.vars,
  };
  const url = URL.createObjectURL(new Blob([JSON.stringify(payload, null, 2)], {
    type: 'application/json',
  }));
  const a = document.createElement('a');
  a.href = url;
  a.download = `${(theme.name || 'theme').replace(/[^\w-]+/g, '-').toLowerCase()}.quarkdrive-theme.json`;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 20000);
}

/* ------------------------------------------------------------ wiring */

function selectTab(which) {
  $('#tab-files').classList.toggle('active', which === 'files');
  $('#tab-photos').classList.toggle('active', which === 'photos');
  $('#tab-files').setAttribute('aria-selected', String(which === 'files'));
  $('#tab-photos').setAttribute('aria-selected', String(which === 'photos'));
  $('#files-view').hidden = which !== 'files';
  $('#photos-view').hidden = which !== 'photos';
}

async function start() {
  showApp();
  try {
    await loadVaults();
    await loadFiles();
    await loadTimeline().catch(() => { state.photos = []; });
  } catch (e) {
    toast(e.message, 'error');
    setStatus(e.message);
  }
}

function wireThemes() {
  $('#btn-theme').addEventListener('click', openThemeDrawer);
  $('#theme-close').addEventListener('click', closeThemeDrawer);
  $('#theme-scrim').addEventListener('click', closeThemeDrawer);

  $('#btn-new-theme').addEventListener('click', () => {
    openThemeEditor(QuarkdriveThemes.get(Themes.currentId()));
  });

  $('#btn-import-theme').addEventListener('click', () => $('#import-file').click());
  $('#import-file').addEventListener('change', async (ev) => {
    const file = ev.target.files && ev.target.files[0];
    ev.target.value = '';
    if (!file) return;
    try {
      const theme = QuarkdriveThemes.parseImported(await file.text());
      Themes.saveCustom(theme);
      Themes.apply(theme.id);
      renderThemeList();
      toast(`Imported “${theme.name}”`, 'ok');
    } catch (e) {
      toast(`Import failed: ${e.message}`, 'error');
    }
  });

  $('#btn-reset-theme').addEventListener('click', () => {
    Themes.apply(QuarkdriveThemes.DEFAULT_ID);
    renderThemeList();
    toast('Theme reset to Galaxy', 'ok');
  });

  // Editor
  $('#te-close').addEventListener('click', closeThemeEditor);
  $('#te-cancel').addEventListener('click', closeThemeEditor);
  $('#te-save').addEventListener('click', saveThemeEditor);
  $('#te-delete').addEventListener('click', deleteThemeEditor);

  $('#te-base').addEventListener('change', (ev) => {
    // Re-derive the draft from the chosen base, keeping the name being typed.
    const name = $('#te-name').value.trim() || 'My theme';
    const draft = QuarkdriveThemes.derive(QuarkdriveThemes.get(ev.target.value),
      QuarkdriveThemes.suggestId(name), name);
    draft.backgroundImage = Themes.editing ? Themes.editing.backgroundImage : '';
    Themes.editing = draft;
    buildColourFields(draft.vars);
  });

  $('#te-name').addEventListener('input', () => {
    if (Themes.editing) Themes.editing.name = $('#te-name').value.trim();
  });

  $('#te-bg-url').addEventListener('change', () => {
    const url = $('#te-bg-url').value.trim();
    if (!Themes.editing) return;
    Themes.editing.backgroundImage = url || '';
    Themes.preview(Themes.editing);
    refreshPreview();
  });

  $('#te-bg-file').addEventListener('click', () => $('#te-bg-file-input').click());
  $('#te-bg-file-input').addEventListener('change', async (ev) => {
    const file = ev.target.files && ev.target.files[0];
    ev.target.value = '';
    if (!file || !Themes.editing) return;
    try {
      const dataUrl = await sizedDataUrl(file);
      if (dataUrl.length > 2_500_000) {
        throw new Error('that image is still too large after shrinking — try a smaller one');
      }
      Themes.editing.backgroundImage = dataUrl;
      Themes.preview(Themes.editing);
      refreshPreview();
    } catch (e) {
      toast(`Background not set: ${e.message}`, 'error');
    }
  });

  $('#te-bg-clear').addEventListener('click', () => {
    if (!Themes.editing) return;
    Themes.editing.backgroundImage = '';
    $('#te-bg-url').value = '';
    Themes.preview(Themes.editing);
    refreshPreview();
  });

  $('#te-export').addEventListener('click', () => {
    if (Themes.editing) exportTheme(Themes.editing);
  });

  document.addEventListener('keydown', (ev) => {
    if (ev.key !== 'Escape') return;
    if (!$('#theme-editor').hidden) closeThemeEditor();
    else if (!$('#theme-drawer').hidden) closeThemeDrawer();
  });
}

function init() {
  // Theme first, so the page never flashes the fallback palette.
  Themes.applyCurrent();
  wireThemes();

  const loginError = $('#login-error');
  $('#login-form').addEventListener('submit', async (ev) => {
    ev.preventDefault();
    loginError.hidden = true;
    const unreachable = (e) =>
      e instanceof TypeError || e.name === 'TypeError'
        ? 'Could not reach the server. Check the address, and that the server is running and reachable from this browser.'
        : null;

    if (!$('#signup-fields').hidden) {
      const server = $('#signup-server').value.trim();
      const username = $('#signup-username').value.trim();
      const password = $('#signup-password').value;
      const confirm = $('#signup-confirm').value;
      const vault = $('#signup-vault').value.trim();
      $('#signup-submit').disabled = true;
      try {
        if (password.length < 6) throw new Error('Password must be at least 6 characters.');
        if (password !== confirm) throw new Error('The two passwords do not match.');
        if (!username) throw new Error('Pick a username.');
        await register(username, password, vault, server);
        await start();
      } catch (e) {
        loginError.textContent = unreachable(e) || e.message || 'could not create the account';
        loginError.hidden = false;
      } finally {
        $('#signup-submit').disabled = false;
      }
      return;
    }

    const server = $('#login-server').value.trim();
    const username = $('#login-username').value.trim();
    const password = $('#login-password').value;
    $('#login-submit').disabled = true;
    try {
      await signIn(server, username, password);
      await start();
    } catch (e) {
      // A fetch that never reached the server throws a TypeError with a
      // browser-specific message, which does not help distinguish "wrong
      // password" from "wrong address".
      loginError.textContent = unreachable(e) || e.message || 'sign in failed';
      loginError.hidden = false;
    } finally {
      $('#login-submit').disabled = false;
    }
  });

  $('#to-signup').addEventListener('click', (ev) => {
    ev.preventDefault();
    showSignup();
  });
  $('#to-signin').addEventListener('click', (ev) => {
    ev.preventDefault();
    showSignin();
  });
  $('#signup-username').addEventListener('input', () => {
    $('#signup-vault').placeholder = $('#signup-username').value.trim() || 'photos';
  });

  $('#logout').addEventListener('click', signOut);

  $('#vault-select').addEventListener('change', async (ev) => {
    state.vault = ev.target.value;
    localStorage.setItem(STORAGE.vault, state.vault);
    state.path = '';
    try {
      await Promise.all([loadFiles(), loadTimeline()]);
    } catch (e) {
      toast(e.message, 'error');
    }
  });

  $('#tab-files').addEventListener('click', () => selectTab('files'));
  $('#tab-photos').addEventListener('click', () => selectTab('photos'));

  $('#btn-upload').addEventListener('click', () => $('#file-input').click());
  $('#file-input').addEventListener('change', (ev) => {
    uploadFiles(ev.target.files);
    ev.target.value = '';
  });
  $('#btn-newfolder').addEventListener('click', createFolder);
  $('#btn-new-vault').addEventListener('click', createVault);

  $('#filter-input').addEventListener('input', (ev) => {
    state.filter = ev.target.value;
    renderFiles();
  });

  const zone = $('#dropzone');
  const hint = $('#drop-hint');
  const showHint = (on) => {
    zone.classList.toggle('dragover', on);
    hint.hidden = !on;
  };
  ['dragenter', 'dragover'].forEach((type) =>
    zone.addEventListener(type, (ev) => { ev.preventDefault(); showHint(true); }));
  ['dragleave', 'drop'].forEach((type) =>
    zone.addEventListener(type, (ev) => { ev.preventDefault(); showHint(false); }));
  zone.addEventListener('drop', (ev) => {
    if (ev.dataTransfer && ev.dataTransfer.files.length) uploadFiles(ev.dataTransfer.files);
  });

  $('#lb-close').addEventListener('click', closeLightbox);
  $('#lb-prev').addEventListener('click', () => stepLightbox(-1));
  $('#lb-next').addEventListener('click', () => stepLightbox(1));
  $('#lightbox').addEventListener('click', (ev) => {
    if (ev.target === $('#lightbox')) closeLightbox();
  });

  document.addEventListener('keydown', (ev) => {
    if ($('#lightbox').hidden) return;
    if (ev.key === 'Escape') closeLightbox();
    else if (ev.key === 'ArrowLeft') stepLightbox(-1);
    else if (ev.key === 'ArrowRight') stepLightbox(1);
  });

  selectTab('files');

  // Resume a saved session, if it is still valid.
  if (state.server && state.token) {
    api('/api/v1/whoami')
      .then(start)
      .catch(() => showLogin());
  } else {
    showLogin();
  }
}

document.addEventListener('DOMContentLoaded', init);
