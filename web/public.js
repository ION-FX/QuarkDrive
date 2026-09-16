/* The public share-link page: what a visitor sees at /s/<id>.
 *
 * Deliberately separate from app.js — no session, no vault picker, no
 * upload. It reads the link id from the URL and talks only to the
 * /api/v1/public endpoints. A password, if the link has one, stays in
 * this tab (sessionStorage) and travels in an X-Link-Password header so
 * it never appears in a URL.
 */

'use strict';

const linkId = decodeURIComponent(
  window.location.pathname.split('/').pop() || '',
);
let password = sessionStorage.getItem('qd.link.' + linkId) || '';
let currentPath = '';

const $ = (sel) => document.querySelector(sel);

function toast(message, kind) {
  const box = document.createElement('div');
  box.className = 'toast' + (kind === 'error' ? ' toast-error' : '');
  box.textContent = message;
  $('#toasts').appendChild(box);
  setTimeout(() => box.remove(), 4000);
}

function headers(extra) {
  const h = extra || {};
  if (password) h['X-Link-Password'] = password;
  return h;
}

async function call(pathname) {
  const res = await fetch('/api/v1/public/' + encodeURIComponent(linkId) + pathname, {
    headers: headers(),
  });
  if (res.status === 401) {
    $('#pub-password').hidden = false;
    throw new Error('this link needs a password');
  }
  if (res.status === 410) throw new Error('this link has expired');
  if (res.status === 404) throw new Error('nothing is shared here any more');
  if (!res.ok) {
    const data = await res.json().catch(() => ({}));
    throw new Error(data.error || res.statusText);
  }
  return res;
}

function showStatus(message) {
  const el = $('#pub-status');
  el.textContent = message;
  el.hidden = false;
}

async function saveBlob(pathname, filename) {
  const res = await call(pathname);
  const blob = await res.blob();
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 20000);
}

function entryRow(item) {
  const li = document.createElement('li');
  li.className = 'entry';

  const icon = document.createElement('span');
  icon.className = 'entry-icon';
  icon.textContent = item.kind === 'dir' ? '📁' : '📄';

  const name = document.createElement('button');
  name.className = 'entry-name';
  name.textContent = item.name;

  const meta = document.createElement('span');
  meta.className = 'entry-meta';
  meta.textContent = item.kind === 'dir'
    ? 'folder'
    : (item.size / 1024).toFixed(item.size < 10240 ? 1 : 0) + ' KB';

  if (item.kind === 'dir') {
    name.addEventListener('click', () => {
      currentPath = item.path;
      loadList().catch((e) => toast(e.message, 'error'));
    });
  } else {
    name.addEventListener('click', () =>
      saveBlob(
        '/download?path=' + encodeURIComponent(item.path),
        item.name,
      ).catch((e) => toast(e.message, 'error')));
    const get = document.createElement('button');
    get.className = 'ghost';
    get.textContent = 'Download';
    get.addEventListener('click', () =>
      saveBlob(
        '/download?path=' + encodeURIComponent(item.path),
        item.name,
      ).catch((e) => toast(e.message, 'error')));
    li.appendChild(get);
  }

  li.append(icon, name, meta);
  return li;
}

async function loadList() {
  $('#pub-list').innerHTML = '';
  const res = await call('/list?path=' + encodeURIComponent(currentPath));
  const data = await res.json();

  if (currentPath) {
    const up = document.createElement('li');
    up.className = 'entry';
    const back = document.createElement('button');
    back.className = 'entry-name';
    back.textContent = '← up one folder';
    const parent = currentPath.includes('/')
      ? currentPath.slice(0, currentPath.lastIndexOf('/'))
      : '';
    back.addEventListener('click', () => {
      currentPath = parent;
      loadList().catch((e) => toast(e.message, 'error'));
    });
    up.appendChild(back);
    $('#pub-list').appendChild(up);
  }

  if (!data.items.length) {
    showStatus('This folder is empty.');
    return;
  }
  $('#pub-status').hidden = true;
  for (const item of data.items) $('#pub-list').appendChild(entryRow(item));
}

async function open() {
  try {
    const res = await call('');
    const meta = await res.json();

    $('#pub-password').hidden = true;
    $('#pub-title').textContent =
      (meta.name || 'Shared') + ' — shared with you';

    if (meta.kind === 'file') {
      $('#pub-file').hidden = false;
      $('#pub-file-name').textContent = meta.name;
      $('#pub-download').addEventListener('click', () =>
        saveBlob('/download?path=', meta.name).catch((e) => toast(e.message, 'error')));
    } else {
      await loadList();
    }
  } catch (e) {
    if ($('#pub-password').hidden) showStatus(e.message);
  }
}

$('#pub-unlock').addEventListener('click', () => {
  password = $('#pub-pass').value;
  sessionStorage.setItem('qd.link.' + linkId, password);
  open();
});
$('#pub-pass').addEventListener('keydown', (ev) => {
  if (ev.key === 'Enter') {
    ev.preventDefault();
    $('#pub-unlock').click();
  }
});

open();
