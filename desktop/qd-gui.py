#!/usr/bin/env python3
"""Quarkdrive desktop GUI (Linux, PyQt6).

A native desktop client for a Quarkdrive vault: browse files, upload and
download, rename or move, make folders and vaults, and browse the photo
timeline with server-generated thumbnails.

It talks to the same file API as the web UI and the Android app, so it does
not need the Rust core locally — the server does the chunking, hashing and
storage. Automatic background sync stays with `qd watch`; this window is the
human-facing half.

Run it (PyQt6 installed into the user site):

    pip install --user --break-system-packages PyQt6   # Ubuntu 24.04
    python3 desktop/qd-gui.py

Everything runs offscreen too, which is how desktop/test-gui.py verifies the
app and takes screenshots on a machine with no display server:

    QT_QPA_PLATFORM=offscreen python3 desktop/test-gui.py
"""

import datetime
import json
import os
import sys
import urllib.error
import urllib.request

from PyQt6.QtCore import QSize, Qt, QThread, pyqtSignal
from PyQt6.QtGui import QColor, QIcon, QPalette, QPixmap
from PyQt6.QtWidgets import (QApplication, QComboBox, QDialog, QFileDialog,
                             QFormLayout, QHBoxLayout, QHeaderView,
                             QInputDialog, QLabel, QLineEdit, QListWidget,
                             QListWidgetItem, QMainWindow, QMenu,
                             QMessageBox, QPushButton, QScrollArea,
                             QStatusBar, QStyle, QTabWidget, QTreeWidget,
                             QTreeWidgetItem, QVBoxLayout, QWidget)

APP_NAME = 'Quarkdrive'
CONFIG_FILE = os.path.expanduser('~/.config/quarkdrive/gui.json')
REQUEST_TIMEOUT = 60
TRANSFER_TIMEOUT = 600
MAX_PHOTOS = 300
THUMB_SIZE = 168


def human_size(n):
    if n < 1024:
        return f'{n} B'
    units = ['KiB', 'MiB', 'GiB', 'TiB']
    value = float(n)
    for unit in units:
        value /= 1024.0
        if value < 1024 or unit == units[-1]:
            return f'{value:.1f} {unit}'


def load_session():
    try:
        with open(CONFIG_FILE, encoding='utf-8') as f:
            cfg = json.load(f)
        if cfg.get('server') and cfg.get('token') and cfg.get('vault'):
            return cfg
    except (OSError, ValueError):
        pass
    return None


def save_session(cfg):
    os.makedirs(os.path.dirname(CONFIG_FILE), exist_ok=True)
    with open(CONFIG_FILE, 'w', encoding='utf-8') as f:
        json.dump(cfg, f, indent=2)
    os.chmod(CONFIG_FILE, 0o600)


def clear_session():
    try:
        os.remove(CONFIG_FILE)
    except OSError:
        pass


# --------------------------------------------------------------------- api

class ApiError(Exception):
    """A failed request, carrying a message meant for a human."""


class Api:
    """Client for the server's file API (the same one the web UI uses)."""

    def __init__(self, server, token, vault):
        self.server = server.rstrip('/')
        self.token = token
        self.vault = vault

    # ---- plumbing

    def _request(self, method, path, data=None, headers=None, timeout=REQUEST_TIMEOUT):
        req = urllib.request.Request(self.server + path, data=data, method=method)
        if self.token:
            req.add_header('Authorization', 'Bearer ' + self.token)
        for key, value in (headers or {}).items():
            req.add_header(key, value)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                return resp.read()
        except urllib.error.HTTPError as e:
            body = e.read().decode('utf-8', 'replace')
            try:
                message = json.loads(body).get('error', body)
            except ValueError:
                message = body
            raise ApiError(message or f'HTTP {e.code}') from None
        except urllib.error.URLError as e:
            raise ApiError(
                f'cannot reach {self.server} — is the server running? ({e.reason})'
            ) from None

    def _json(self, method, path, payload=None):
        data = None
        headers = {}
        if payload is not None:
            data = json.dumps(payload).encode('utf-8')
            headers['Content-Type'] = 'application/json'
        return json.loads(self._request(method, path, data, headers))

    def _vault_path(self, suffix, query=''):
        from urllib.parse import quote
        return f'/api/v1/vaults/{quote(self.vault)}{suffix}' + (f'?{query}' if query else '')

    # ---- session

    @staticmethod
    def login(server, username, password):
        payload = json.dumps({'username': username, 'password': password}).encode('utf-8')
        req = urllib.request.Request(
            server.rstrip('/') + '/api/v1/auth/login', data=payload, method='POST')
        req.add_header('Content-Type', 'application/json')
        try:
            with urllib.request.urlopen(req, timeout=REQUEST_TIMEOUT) as resp:
                return json.loads(resp.read()).get('token')
        except urllib.error.HTTPError as e:
            body = e.read().decode('utf-8', 'replace')
            try:
                message = json.loads(body).get('error', body)
            except ValueError:
                message = body
            raise ApiError(message or 'login failed') from None
        except urllib.error.URLError as e:
            raise ApiError(
                f'cannot reach {server} — is the server running? ({e.reason})'
            ) from None

    def vaults(self):
        return [v['name'] for v in self._json('GET', '/api/v1/vaults')['vaults']]

    def create_vault(self, name):
        return self._json('POST', '/api/v1/vaults', {'name': name})

    # ---- files

    def list(self, path=''):
        query = ''
        if path:
            from urllib.parse import quote
            query = 'path=' + quote(path)
        return self._json('GET', self._vault_path('/fs', query))['entries']

    def mkdir(self, path):
        from urllib.parse import quote
        self._request('POST', self._vault_path('/fs/mkdir', 'path=' + quote(path)),
                      data=b'{}', headers={'Content-Type': 'application/json'})

    def upload(self, path, data):
        from urllib.parse import quote
        self._request('PUT', self._vault_path('/fs', 'path=' + quote(path)), data,
                      timeout=TRANSFER_TIMEOUT)

    def delete(self, path):
        from urllib.parse import quote
        self._request('DELETE', self._vault_path('/fs', 'path=' + quote(path)))

    def move(self, src, dst):
        from urllib.parse import quote
        query = f'from={quote(src)}&to={quote(dst)}'
        self._request('POST', self._vault_path('/fs/move', query), data=b'{}')

    def download(self, path):
        from urllib.parse import quote
        return self._request('GET', self._vault_path('/fs/download', 'path=' + quote(path)),
                             timeout=TRANSFER_TIMEOUT)

    # ---- photos

    def timeline(self, limit=MAX_PHOTOS):
        return self._json('GET', self._vault_path('/timeline', f'limit={limit}'))['items']

    def thumb(self, server_relative):
        return self._request('GET', server_relative)

    # ---- info

    def stats(self):
        return self._json('GET', self._vault_path('/stats'))


# ------------------------------------------------------------------ worker

class Worker(QThread):
    """Runs one API call off the UI thread and reports back by signal."""

    done = pyqtSignal(object)
    failed = pyqtSignal(str)

    def __init__(self, fn, *args):
        super().__init__()
        self._fn = fn
        self._args = args

    def run(self):
        try:
            self.done.emit(self._fn(*self._args))
        except Exception as e:  # noqa: BLE001 - surfaced to the user either way
            self.failed.emit(str(e) or e.__class__.__name__)


def apply_dark_theme(app):
    """A galaxy-dark palette in the spirit of the web UI's default theme."""
    app.setStyle('Fusion')
    window = QColor(0x14, 0x10, 0x22)
    base = QColor(0x1b, 0x16, 0x2e)
    alternate = QColor(0x24, 0x1d, 0x3c)
    text = QColor(0xe8, 0xe4, 0xf2)
    dim = QColor(0x9a, 0x92, 0xb0)
    accent = QColor(0x8a, 0x5c, 0xff)

    palette = QPalette()
    palette.setColor(QPalette.ColorRole.Window, window)
    palette.setColor(QPalette.ColorRole.WindowText, text)
    palette.setColor(QPalette.ColorRole.Base, base)
    palette.setColor(QPalette.ColorRole.AlternateBase, alternate)
    palette.setColor(QPalette.ColorRole.Text, text)
    palette.setColor(QPalette.ColorRole.Button, base)
    palette.setColor(QPalette.ColorRole.ButtonText, text)
    palette.setColor(QPalette.ColorRole.Highlight, accent)
    palette.setColor(QPalette.ColorRole.HighlightedText, QColor(0xff, 0xff, 0xff))
    palette.setColor(QPalette.ColorRole.PlaceholderText, dim)
    palette.setColor(QPalette.ColorRole.ToolTipBase, base)
    palette.setColor(QPalette.ColorRole.ToolTipText, text)
    disabled = QColor(0x6b, 0x64, 0x80)
    palette.setColorGroup(QPalette.ColorGroup.Disabled,
                          window, dim, base, alternate, dim, base, dim,
                          QColor(0x2a, 0x24, 0x44), text)
    app.setPalette(palette)


# ------------------------------------------------------------ login dialog

class LoginDialog(QDialog):
    """Server / username / password — the same flow as the web UI."""

    def __init__(self, parent=None):
        super().__init__(parent)
        self.setWindowTitle(APP_NAME)
        self.setMinimumWidth(380)
        self.session = None

        form = QFormLayout(self)
        form.setSpacing(12)

        self.server = QLineEdit('http://localhost:8787')
        self.username = QLineEdit()
        self.username.setPlaceholderText('ada')
        self.password = QLineEdit()
        self.password.setEchoMode(QLineEdit.EchoMode.Password)

        form.addRow('Server', self.server)
        form.addRow('Username', self.username)
        form.addRow('Password', self.password)

        self.error_label = QLabel('')
        self.error_label.setStyleSheet('color: #ff7b8b;')
        self.error_label.setWordWrap(True)
        form.addRow(self.error_label)

        self.connect_button = QPushButton('Sign in')
        self.connect_button.setDefault(True)
        self.connect_button.clicked.connect(self.sign_in)
        form.addRow(self.connect_button)

        self._workers = set()

    def sign_in(self):
        server = self.server.text().strip()
        username = self.username.text().strip()
        password = self.password.text()
        if not server or not username or not password:
            self.error_label.setText('Fill in all three fields.')
            return
        self._set_busy(True)
        self.error_label.setText('')
        self._start(self._connect, server)

    def _connect(self, server):
        token = Api.login(server, self.username.text().strip(), self.password.text())
        api = Api(server, token, '')
        vaults = api.vaults()
        if not vaults:
            raise ApiError('this account has no vaults yet — create one with '
                           '`quarkdrive-server create-vault`')
        return {'server': server.rstrip('/'), 'token': token, 'vault': vaults[0]}

    def _start(self, fn, *args):
        worker = Worker(fn, *args)
        worker.done.connect(self._succeeded)
        worker.failed.connect(self._failed)
        worker.finished.connect(lambda: self._workers.discard(worker))
        self._workers.add(worker)
        worker.start()

    def _succeeded(self, session):
        self._set_busy(False)
        self.session = session
        self.accept()

    def _failed(self, message):
        self._set_busy(False)
        self.error_label.setText(message)

    def _set_busy(self, busy):
        self.connect_button.setEnabled(not busy)
        self.connect_button.setText('Signing in…' if busy else 'Sign in')


# ------------------------------------------------------------- main window

class MainWindow(QMainWindow):
    def __init__(self, session):
        super().__init__()
        self.session = session
        self.api = Api(session['server'], session['token'], session['vault'])
        self.path = ''
        self._workers = set()
        self._busy = 0
        self._pending_thumbs = 0

        self.setWindowTitle(f'{APP_NAME} — {self.api.vault}')
        self.resize(960, 640)
        self.setAcceptDrops(True)

        root = QWidget()
        layout = QVBoxLayout(root)
        layout.setContentsMargins(8, 8, 8, 8)
        layout.setSpacing(8)

        # vault row
        vault_row = QHBoxLayout()
        vault_row.addWidget(QLabel('Vault'))
        self.vault_select = QComboBox()
        self.vault_select.currentTextChanged.connect(self.switch_vault)
        vault_row.addWidget(self.vault_select, stretch=0)
        new_vault_btn = QPushButton('New vault…')
        new_vault_btn.clicked.connect(self.new_vault)
        vault_row.addWidget(new_vault_btn)
        vault_row.addStretch(1)
        self.stats_label = QLabel('')
        self.stats_label.setStyleSheet('color: #9a92b0;')
        vault_row.addWidget(self.stats_label)
        layout.addLayout(vault_row)

        # toolbar row
        toolbar = QHBoxLayout()
        self.up_button = QPushButton('Up')
        self.up_button.clicked.connect(self.go_up)
        toolbar.addWidget(self.up_button)
        self.path_label = QLabel('/')
        self.path_label.setStyleSheet('color: #b8a8e8;')
        toolbar.addWidget(self.path_label, stretch=1)
        self.filter_box = QLineEdit()
        self.filter_box.setPlaceholderText('Filter…')
        self.filter_box.setMaximumWidth(220)
        self.filter_box.textChanged.connect(self.apply_filter)
        toolbar.addWidget(self.filter_box)
        upload_btn = QPushButton('Upload…')
        upload_btn.clicked.connect(self.pick_uploads)
        toolbar.addWidget(upload_btn)
        mkdir_btn = QPushButton('New folder…')
        mkdir_btn.clicked.connect(self.new_folder)
        toolbar.addWidget(mkdir_btn)
        layout.addLayout(toolbar)

        # files + photos
        self.tabs = QTabWidget()
        self.tree = QTreeWidget()
        self.tree.setColumnCount(3)
        self.tree.setHeaderLabels(['Name', 'Size', 'Modified'])
        self.tree.setRootIsDecorated(False)
        self.tree.setAlternatingRowColors(True)
        self.tree.header().setStretchLastSection(False)
        self.tree.header().setSectionResizeMode(0, QHeaderView.ResizeMode.Stretch)
        self.tree.header().setSectionResizeMode(1, QHeaderView.ResizeMode.ResizeToContents)
        self.tree.header().setSectionResizeMode(2, QHeaderView.ResizeMode.ResizeToContents)
        self.tree.itemDoubleClicked.connect(self.open_entry)
        self.tree.setContextMenuPolicy(Qt.ContextMenuPolicy.CustomContextMenu)
        self.tree.customContextMenuRequested.connect(self.entry_menu)
        self.tabs.addTab(self.tree, 'Files')

        self.photos = QListWidget()
        self.photos.setViewMode(QListWidget.ViewMode.IconMode)
        self.photos.setIconSize(QSize(THUMB_SIZE, THUMB_SIZE))
        self.photos.setResizeMode(QListWidget.ResizeMode.Adjust)
        self.photos.setMovement(QListWidget.Movement.Static)
        self.photos.setSpacing(10)
        self.photos.setWordWrap(True)
        self.photos.itemDoubleClicked.connect(self.preview_photo)
        self.tabs.addTab(self.photos, 'Photos')
        self.tabs.currentChanged.connect(lambda _: self.reload_current_tab())
        layout.addWidget(self.tabs, stretch=1)

        self.setCentralWidget(root)
        self.statusBar().showMessage('')

    # ------------------------------------------------------- worker helpers

    def _start(self, fn, on_done, on_fail=None, *args):
        """Run one API call in a worker thread.

        Busy accounting lives here — on worker finish the counter drops and
        the status bar clears — so no callback has to remember to do it.
        """
        worker = Worker(fn, *args)
        worker.done.connect(on_done)
        worker.failed.connect(on_fail or self.show_error)

        def finished():
            self._workers.discard(worker)
            self._busy = max(0, self._busy - 1)
            if self._busy == 0:
                self.statusBar().showMessage('', 1500)

        worker.finished.connect(finished)
        self._workers.add(worker)
        self._busy += 1
        self.statusBar().showMessage('Working…')
        worker.start()

    def show_error(self, message):
        QMessageBox.critical(self, APP_NAME, str(message))

    # ---------------------------------------------------------- lifecycle

    def start(self):
        self._start(self.api.vaults, self.set_vaults)
        self.reload_files()

    def set_vaults(self, names):
        self.vault_select.blockSignals(True)
        self.vault_select.clear()
        self.vault_select.addItems(names)
        if self.api.vault in names:
            self.vault_select.setCurrentText(self.api.vault)
        self.vault_select.blockSignals(False)
        self.refresh_stats()

    def switch_vault(self, name):
        if not name or name == self.api.vault:
            return
        self.api = Api(self.api.server, self.api.token, name)
        self.session['vault'] = name
        save_session(self.session)
        self.setWindowTitle(f'{APP_NAME} — {name}')
        self.path = ''
        self.reload_files()
        if self.tabs.currentIndex() == 1:
            self.reload_photos()
        self.refresh_stats()

    def new_vault(self):
        name, ok = QInputDialog.getText(self, 'New vault', 'Vault name:')
        if not ok or not name.strip():
            return
        self._start(self.api.create_vault, lambda _: self.start(), None, name.strip())

    def refresh_stats(self):
        def apply(st):
            self.stats_label.setText(
                f"{st['files']} files · {human_size(st['bytes'])}")
        self._start(self.api.stats, apply)

    # ------------------------------------------------------------- files

    def reload_files(self):
        self.path_label.setText('/' + self.path)
        self._start(self.api.list, self.populate_files, None, self.path)

    def populate_files(self, entries):
        self.tree.clear()
        dirs = sorted((e for e in entries if e['kind'] == 'dir'), key=lambda e: e['name'])
        files = sorted((e for e in entries if e['kind'] != 'dir'), key=lambda e: e['name'])
        for entry in dirs + files:
            item = QTreeWidgetItem([entry['name'],
                                    '' if entry['kind'] == 'dir' else human_size(entry['size']),
                                    datetime.datetime.fromtimestamp(
                                        entry.get('mtime', 0)).strftime('%Y-%m-%d %H:%M')])
            style = self.style()
            icon = style.standardIcon(
                QStyle.StandardPixmap.SP_DirIcon if entry['kind'] == 'dir'
                else QStyle.StandardPixmap.SP_FileIcon)
            item.setIcon(0, icon)
            item.setData(0, Qt.ItemDataRole.UserRole, entry)
            self.tree.addTopLevelItem(item)
        self.apply_filter()

    def apply_filter(self):
        needle = self.filter_box.text().strip().lower()
        for i in range(self.tree.topLevelItemCount()):
            item = self.tree.topLevelItem(i)
            item.setHidden(bool(needle and needle not in item.text(0).lower()))
        for i in range(self.photos.count()):
            item = self.photos.item(i)
            item.setHidden(bool(needle and needle not in item.text().lower()))

    def open_entry(self, item, _col):
        entry = item.data(0, Qt.ItemDataRole.UserRole)
        if entry['kind'] == 'dir':
            self.path = entry['path']
            self.reload_files()
        else:
            self.save_entry(entry)

    def go_up(self):
        if '/' in self.path:
            self.path = self.path.rsplit('/', 1)[0]
        else:
            self.path = ''
        self.reload_files()

    def reload_current_tab(self, *_):
        if self.tabs.currentIndex() == 1:
            self.reload_photos()

    # ------------------------------------------------------ file actions

    def selected_entry(self):
        items = self.tree.selectedItems()
        return items[0].data(0, Qt.ItemDataRole.UserRole) if items else None

    def entry_menu(self, pos):
        entry = self.selected_entry()
        if entry is None:
            return
        menu = QMenu(self)
        if entry['kind'] == 'dir':
            menu.addAction('Open', lambda: self.open_entry(self.tree.selectedItems()[0], 0))
        else:
            menu.addAction('Download…', lambda: self.save_entry(entry))
        menu.addAction('Rename / move…', lambda: self.rename_entry(entry))
        menu.addSeparator()
        menu.addAction('Delete', lambda: self.delete_entry(entry))
        menu.exec(self.tree.viewport().mapToGlobal(pos))

    def save_entry(self, entry):
        target, _ = QFileDialog.getSaveFileName(self, 'Save as', entry['name'])
        if not target:
            return

        def write(data):
            with open(target, 'wb') as f:
                f.write(data)
            self.statusBar().showMessage(f'Saved {target}', 4000)

        self._start(self.api.download, write, None, entry['path'])

    def rename_entry(self, entry):
        name, ok = QInputDialog.getText(
            self, 'Rename / move', 'New name — use a path to move it into a folder:',
            text=entry['name'])
        if not ok or not name.strip() or name.strip() == entry['name']:
            return
        target = name.strip().lstrip('/')
        parent = entry['path'].rsplit('/', 1)[0] if '/' in entry['path'] else ''
        dst = f'{parent}/{target}' if parent else target
        self._start(self.api.move,
                    lambda _: (self.reload_files(), self.refresh_stats()),
                    None, entry['path'], dst)

    def delete_entry(self, entry):
        answer = QMessageBox.question(
            self, 'Delete', f'Delete {entry["path"]}?',
            QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No)
        if answer != QMessageBox.StandardButton.Yes:
            return
        self._start(self.api.delete,
                    lambda _: (self.reload_files(), self.refresh_stats()),
                    None, entry['path'])

    def new_folder(self):
        name, ok = QInputDialog.getText(self, 'New folder', 'Folder name:')
        if not ok or not name.strip():
            return
        path = f'{self.path}/{name.strip()}' if self.path else name.strip()
        self._start(self.api.mkdir,
                    lambda _: (self.reload_files(), self.refresh_stats()),
                    None, path)

    def pick_uploads(self):
        paths, _ = QFileDialog.getOpenFileNames(self, 'Upload files')
        self.upload_paths(paths)

    def upload_paths(self, paths):
        for local in paths:
            name = os.path.basename(local)
            remote = f'{self.path}/{name}' if self.path else name
            try:
                with open(local, 'rb') as f:
                    data = f.read()
            except OSError as e:
                QMessageBox.warning(self, APP_NAME, f'Cannot read {local}: {e}')
                continue
            self._start(self.api.upload,
                        lambda _=None: (self.reload_files(), self.refresh_stats()),
                        None, remote, data)

    # ------------------------------------------------------------ photos

    def reload_photos(self):
        self.photos.clear()
        self._pending_thumbs = 0
        self._start(self._fetch_thumbs, self.populate_photos)

    def _fetch_thumbs(self):
        """Worker: timeline entries plus their thumbnail bytes."""
        out = []
        for entry in self.api.timeline():
            data = None
            if entry.get('thumb'):
                try:
                    data = self.api.thumb(entry['thumb'])
                except ApiError:
                    data = None
            entry['_thumb_bytes'] = data
            out.append(entry)
        return out

    def populate_photos(self, items):
        for entry in items:
            name = entry['path'].rsplit('/', 1)[-1]
            when = datetime.datetime.fromtimestamp(
                entry.get('taken_at', 0)).strftime('%Y-%m-%d')
            item = QListWidgetItem(f'{name}\n{when}')
            item.setData(Qt.ItemDataRole.UserRole, entry)
            thumb_bytes = entry.get('_thumb_bytes')
            if thumb_bytes:
                pixmap = QPixmap()
                if pixmap.loadFromData(thumb_bytes):
                    item.setIcon(QIcon(pixmap))
            item.setSizeHint(QSize(THUMB_SIZE + 16, THUMB_SIZE + 34))
            self.photos.addItem(item)

    def preview_photo(self, item):
        entry = item.data(0, Qt.ItemDataRole.UserRole)

        def show(data):
            viewer = QDialog(self)
            viewer.setWindowTitle(entry['name'])
            layout = QVBoxLayout(viewer)
            label = QLabel()
            pixmap = QPixmap()
            if pixmap.loadFromData(data):
                label.setPixmap(pixmap.scaled(
                    820, 560, Qt.AspectRatioMode.KeepAspectRatio,
                    Qt.TransformationMode.SmoothTransformation))
            scroll = QScrollArea()
            scroll.setWidget(label)
            layout.addWidget(scroll)
            caption = QLabel(f"{entry['name']} · {human_size(entry['size'])}")
            layout.addWidget(caption)
            viewer.resize(860, 640)
            viewer.exec()

        self._start(self.api.download, show, None, entry['path'])

    # -------------------------------------------------------- drag & drop

    def dragEnterEvent(self, event):
        if event.mimeData().hasUrls():
            event.acceptProposedAction()

    def dropEvent(self, event):
        paths = [url.toLocalFile() for url in event.mimeData().urls()
                 if url.toLocalFile()]
        if paths:
            self.upload_paths(paths)
            event.acceptProposedAction()


# -------------------------------------------------------------------- main

def main():
    app = QApplication(sys.argv)
    app.setApplicationName(APP_NAME)
    apply_dark_theme(app)

    session = load_session()
    window = MainWindow(session) if session else None

    if window is None:
        login = LoginDialog()
        if not login.exec() or not login.session:
            return 0
        save_session(login.session)
        window = MainWindow(login.session)

    window.start()
    window.show()
    return app.exec()


if __name__ == '__main__':
    sys.exit(main())
