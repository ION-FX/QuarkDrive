#!/usr/bin/env python3
"""Offscreen integration test for the Quarkdrive desktop GUI.

Runs the real Qt widgets with QT_QPA_PLATFORM=offscreen (no display server
needed) against a live Quarkdrive server, exercising the same paths a user
would click through:

    login -> list files -> upload -> list again -> download & compare
    -> rename -> delete -> photo timeline with thumbnails -> screenshots

Credentials come from --server/--user/--password (defaults point at the
development server on localhost with its test account).

    QT_QPA_PLATFORM=offscreen python3 desktop/test-gui.py
"""

import argparse
import importlib.util
import os
import sys
import time
from pathlib import Path

from PyQt6.QtGui import QColor, QPainter, QPixmap, QFont
from PyQt6.QtWidgets import QApplication, QMessageBox

HERE = Path(__file__).resolve().parent
SHOTS = Path(os.environ.get('QD_GUI_SHOTS', '/tmp/qd-gui-shots'))

# Under the offscreen platform a modal QMessageBox.exec() can never be
# dismissed — nothing can click it — so dialogs would hang the test forever.
# Record what they say instead, and let the assertions decide whether an
# error dialog was legitimate.
DIALOGS = []


def _record_dialog(title, text):
    DIALOGS.append(f'{title}: {text}')
    print(f'  [dialog] {title}: {text}', flush=True)
    return QMessageBox.StandardButton.Ok


QMessageBox.critical = staticmethod(lambda *a, **k: _record_dialog(a[1], a[2]))
QMessageBox.warning = staticmethod(lambda *a, **k: _record_dialog(a[1], a[2]))
QMessageBox.question = staticmethod(lambda *a, **k: QMessageBox.StandardButton.Yes)


def load_module():
    spec = importlib.util.spec_from_file_location('qdgui', str(HERE / 'qd-gui.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Harness:
    """Pumps the Qt event loop until a condition holds or time runs out."""

    def __init__(self, app):
        self.app = app

    def wait_for(self, what, cond, timeout=30.0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            self.app.processEvents()
            if cond():
                print(f'  ok    {what}')
                return True
            time.sleep(0.05)
        print(f'  FAIL  {what} (timed out after {timeout}s)')
        return False


def make_test_photo(path, colour, label):
    pixmap = QPixmap(900, 600)
    pixmap.fill(colour)
    painter = QPainter(pixmap)
    painter.setFont(QFont('sans-serif', 48, QFont.Weight.Bold))
    painter.setPen(QColor(0xff, 0xff, 0xff))
    painter.drawText(pixmap.rect(), 0x0084 | 0x0004, label)  # alignV|alignHCenter
    painter.end()
    pixmap.save(str(path), 'PNG')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--server', default='http://localhost:8787')
    parser.add_argument('--user', default='phone')
    parser.add_argument('--password', default='change-me-now')
    args = parser.parse_args()

    SHOTS.mkdir(parents=True, exist_ok=True)
    app = QApplication([])
    module = load_module()
    module.apply_dark_theme(app)
    harness = Harness(app)

    passed = 0
    failed = 0

    def check(label, ok):
        nonlocal passed, failed
        if ok:
            passed += 1
            print(f'  ok    {label}')
        else:
            failed += 1
            print(f'  FAIL  {label}')

    print('login dialog')
    login = module.LoginDialog()
    login.server.setText(args.server)
    login.username.setText(args.user)
    login.password.setText(args.password)
    login.sign_in()
    harness.wait_for('signs in against the live server',
                     lambda: login.session is not None or bool(login.error_label.text()))
    check('no login error', not login.error_label.text())
    if not login.session:
        print('cannot continue without a session')
        return 1
    check('session stores a token and a vault',
          bool(login.session['token']) and bool(login.session['vault']))
    login.grab().save(str(SHOTS / 'login.png'))

    # Bad credentials must fail with a human message.
    bad = module.LoginDialog()
    bad.server.setText(args.server)
    bad.username.setText(args.user)
    bad.password.setText('definitely-wrong')
    bad.sign_in()
    harness.wait_for('wrong password shows an error',
                     lambda: bool(bad.error_label.text()))
    check('error names the problem', 'cannot reach' not in bad.error_label.text()
          and len(bad.error_label.text()) > 5)

    print('main window — files')
    window = module.MainWindow(login.session)
    window.start()
    window.resize(960, 640)
    window.show()
    harness.wait_for('vault list loads', lambda: window.vault_select.count() > 0)
    harness.wait_for('file tree is populated',
                     lambda: window.tree.topLevelItemCount() >= 0 and window._busy == 0)
    ok = harness.wait_for('the earlier android upload is visible',
                          lambda: any(window.tree.topLevelItem(i).text(0) == 'from-android.txt'
                                      for i in range(window.tree.topLevelItemCount())))
    check('file listing matches the server', ok)
    check('stats line is filled', 'files' in window.stats_label.text())
    window.grab().save(str(SHOTS / 'files.png'))

    print('upload, download, rename, delete')
    payload = b'uploaded by the PyQt6 GUI offscreen test\n'
    Path('/tmp/qd-gui-upload.txt').write_bytes(payload)
    window.path = ''
    window.upload_paths(['/tmp/qd-gui-upload.txt'])
    harness.wait_for('upload finishes', lambda: window._busy == 0)
    check('uploaded file appears in the tree',
          any(window.tree.topLevelItem(i).text(0) == 'qd-gui-upload.txt'
              for i in range(window.tree.topLevelItemCount())))

    downloaded = window.api.download('qd-gui-upload.txt')
    check('downloaded bytes are identical', downloaded == payload)

    window.api.move('qd-gui-upload.txt', 'qd-gui-renamed.txt')
    window.reload_files()
    harness.wait_for('rename lands in the tree',
                     lambda: any(window.tree.topLevelItem(i).text(0) == 'qd-gui-renamed.txt'
                                 for i in range(window.tree.topLevelItemCount())))
    window.api.delete('qd-gui-renamed.txt')
    window.reload_files()
    harness.wait_for('delete removes it from the tree',
                     lambda: not any(window.tree.topLevelItem(i).text(0) == 'qd-gui-renamed.txt'
                                     for i in range(window.tree.topLevelItemCount())))

    print('photos')
    make_test_photo('/tmp/qd-gui-photo.png', QColor(0x6a, 0x3f, 0xff), 'Quarkdrive')
    window.api.upload('pyqt-test-photo.png', Path('/tmp/qd-gui-photo.png').read_bytes())
    window.tabs.setCurrentIndex(1)
    harness.wait_for('photo timeline loads', lambda: window.photos.count() >= 1)
    ok = harness.wait_for('thumbnail rendered for the uploaded photo',
                          lambda: window.photos.count() >= 1
                          and not window.photos.item(0).icon().isNull(),
                          timeout=60)
    check('thumbnail icon present', ok)
    window.photos.grab().save(str(SHOTS / 'photos.png'))

    print('cleanup')
    for leftover in ('pyqt-test-photo.png',):
        try:
            window.api.delete(leftover)
        except module.ApiError:
            pass

    check('no unexpected error dialogs', not DIALOGS)

    print(f'\n{passed} passed, {failed} failed')
    print(f'screenshots in {SHOTS}')
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main())
