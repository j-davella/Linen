import re
import sys
import os
import pty
import fcntl
import termios
import struct
import select
import subprocess
import time
from pathlib import Path

_ANSI_RE = re.compile(
    r'\x1b(?:[@-Z\\-_]|\[[0-9;]*[A-Za-z]|\][^\x07]*\x07)'
)

_USER_PKGS = Path.home() / '.local' / 'lib' / 'python3.13' / 'site-packages'
if str(_USER_PKGS) not in sys.path and _USER_PKGS.exists():
    sys.path.insert(0, str(_USER_PKGS))

import pyte

from PyQt6.QtWidgets import QWidget, QApplication, QMenu
from PyQt6.QtGui import QPainter, QFont, QFontMetricsF, QColor, QImage, QClipboard
from PyQt6.QtCore import Qt, QThread, pyqtSignal, QTimer

import log
_log = log.get('terminal')

from theme import resolve_color, C
from config import FONT_FAMILY, FONT_SIZE, SCROLLBACK

_PASTE_DIR = Path.home() / '.linen' / 'paste'


class PTYReader(QThread):
    data_ready     = pyqtSignal(bytes)
    process_exited = pyqtSignal()

    def __init__(self, fd):
        super().__init__()
        self._fd = fd
        self._running = True

    def run(self):
        while self._running:
            try:
                r, _, _ = select.select([self._fd], [], [], 0.05)
                if r:
                    data = os.read(self._fd, 65536)
                    if not data:
                        break
                    self.data_ready.emit(data)
            except OSError:
                break
        self.process_exited.emit()

    def stop(self):
        self._running = False


class TerminalWidget(QWidget):
    focus_gained = pyqtSignal()

    def __init__(self, cmd, accent_color=None, parent=None):
        super().__init__(parent)
        self._cmd         = cmd
        self._accent      = accent_color or C['mauve']
        self._scroll_off  = 0
        self._master_fd   = None
        self._process     = None
        self._reader      = None

        self.setFocusPolicy(Qt.FocusPolicy.StrongFocus)
        self.setAttribute(Qt.WidgetAttribute.WA_InputMethodEnabled)
        self.setMouseTracking(True)
        self.setAcceptDrops(True)

        self._font = QFont(FONT_FAMILY, FONT_SIZE)
        self._font.setStyleHint(QFont.StyleHint.Monospace)
        fm = QFontMetricsF(self._font)
        self._cw = fm.horizontalAdvance('M')
        self._ch = fm.height()
        self._asc = fm.ascent()

        self._cols = max(1, int(self.width()  / self._cw)) or 80
        self._rows = max(1, int(self.height() / self._ch)) or 24

        self._screen = pyte.HistoryScreen(self._cols, self._rows, history=SCROLLBACK)
        self._stream = pyte.ByteStream(self._screen)

        # output capture
        self._cap_active   = False
        self._cap_sentinel = ''
        self._cap_buf: list[str] = []

        # text selection
        self._sel_start: tuple[int, int] | None = None
        self._sel_end:   tuple[int, int] | None = None

        self._spawn()

    # ── process management ─────────────────────────────────────────────────

    def _spawn(self):
        master_fd, slave_fd = os.openpty()
        self._master_fd = master_fd

        fcntl.ioctl(slave_fd, termios.TIOCSWINSZ,
                    struct.pack('HHHH', self._rows, self._cols, 0, 0))

        env = os.environ.copy()
        env.update({'TERM': 'xterm-256color', 'COLORTERM': 'truecolor'})

        self._process = subprocess.Popen(
            self._cmd,
            stdin=slave_fd, stdout=slave_fd, stderr=slave_fd,
            env=env,
            preexec_fn=os.setsid,
        )
        os.close(slave_fd)

        self._reader = PTYReader(master_fd)
        self._reader.data_ready.connect(self._on_data)
        self._reader.process_exited.connect(self.update)
        self._reader.start()

    def restart(self, cmd):
        self.shutdown()
        self._cmd = cmd
        self._screen.reset()
        self._scroll_off = 0
        self._spawn()
        self.update()

    def shutdown(self):
        if self._reader:
            self._reader.stop()
            self._reader.wait(1000)
            self._reader = None
        if self._process:
            try:
                self._process.terminate()
                self._process.wait(timeout=2)
            except Exception:
                try:
                    self._process.kill()
                except Exception:
                    pass
            self._process = None
        if self._master_fd is not None:
            try:
                os.close(self._master_fd)
            except OSError:
                pass
            self._master_fd = None

    # ── data handling ──────────────────────────────────────────────────────

    def _on_data(self, data):
        if b'\x1b[c' in data or b'\x1b[0c' in data:
            self._write(b'\x1b[?1;0c')
        if b'\x1b[>c' in data or b'\x1b[>0c' in data:
            self._write(b'\x1b[>0;10;1c')
        self._stream.feed(data)
        if self._cap_active:
            self._cap_buf.append(data.decode('utf-8', errors='replace'))
        if self._scroll_off == 0:
            self.update()

    def _write(self, data: bytes):
        if self._master_fd is not None:
            try:
                os.write(self._master_fd, data)
            except OSError:
                pass

    def send_text(self, text: str):
        self._write(text.encode('utf-8'))

    def get_visible_text(self) -> str:
        lines = []
        for y in range(self._screen.lines):
            row = self._screen.buffer[y]
            line = ''.join(
                row[x].data if x in row else ' '
                for x in range(self._screen.columns)
            )
            lines.append(line.rstrip())
        return '\n'.join(lines)

    def scroll_to_bottom(self):
        self._scroll_off = 0
        self.update()

    # ── capture ────────────────────────────────────────────────────────────

    def start_capture(self, sentinel: str):
        self._cap_buf = []
        self._cap_sentinel = sentinel
        self._cap_active = True
        _log.debug('capture started sentinel=%s', sentinel)

    def stop_capture(self):
        self._cap_active = False
        _log.debug('capture stopped raw_bytes=%d', sum(len(b) for b in self._cap_buf))

    def get_capture(self) -> str:
        raw = ''.join(self._cap_buf)
        return _ANSI_RE.sub('', raw).replace('\r\n', '\n').replace('\r', '\n')

    # ── text selection ─────────────────────────────────────────────────────

    def _pixel_to_cell(self, px: float, py: float) -> tuple[int, int]:
        col = max(0, min(int(px / self._cw), self._screen.columns - 1))
        row = max(0, min(int(py / self._ch), self._screen.lines - 1))
        return row, col

    def _norm_sel(self):
        if not self._sel_start or not self._sel_end:
            return None, None
        r1, c1 = self._sel_start
        r2, c2 = self._sel_end
        if (r1, c1) > (r2, c2):
            r1, c1, r2, c2 = r2, c2, r1, c1
        return (r1, c1), (r2, c2)

    def _in_selection(self, ry: int, x: int) -> bool:
        start, end = self._norm_sel()
        if start is None:
            return False
        return start <= (ry, x) <= end

    def _get_selected_text(self) -> str:
        start, end = self._norm_sel()
        if start is None:
            return ''
        r1, c1 = start
        r2, c2 = end
        rows = self._visible_rows()
        lines = []
        for ry in range(r1, min(r2 + 1, len(rows))):
            row = rows[ry]
            sc = c1 if ry == r1 else 0
            ec = c2 if ry == r2 else self._screen.columns - 1
            line = ''.join(row[x].data if x in row else ' ' for x in range(sc, ec + 1))
            lines.append(line.rstrip())
        return '\n'.join(lines)

    def _select_all(self):
        rows = self._visible_rows()
        if rows:
            self._sel_start = (0, 0)
            self._sel_end = (len(rows) - 1, self._screen.columns - 1)
            text = self._get_selected_text()
            if text:
                QApplication.clipboard().setText(text)
            self.update()

    # ── rendering ──────────────────────────────────────────────────────────

    def _visible_rows(self):
        history = list(self._screen.history.top)
        current = [self._screen.buffer[y] for y in range(self._screen.lines)]
        if self._scroll_off == 0:
            return current
        all_rows = history + current
        end   = max(0, len(all_rows) - self._scroll_off)
        start = max(0, end - self._screen.lines)
        return all_rows[start:end]

    def paintEvent(self, _event):
        p = QPainter(self)
        p.setFont(self._font)
        p.fillRect(self.rect(), QColor(C['base']))

        rows    = self._visible_rows()
        is_live = self._scroll_off == 0

        for ry, row in enumerate(rows):
            for x in range(self._screen.columns):
                try:
                    ch = row[x]
                except (KeyError, IndexError, TypeError):
                    continue

                px = x  * self._cw
                py = ry * self._ch

                fg = resolve_color(ch.fg, C['text'])
                bg = resolve_color(ch.bg, C['base'])
                if ch.reverse:
                    fg, bg = bg, fg

                if self._in_selection(ry, x):
                    bg = QColor(100, 140, 230, 160)

                p.fillRect(int(px), int(py),
                           int(self._cw) + 1, int(self._ch), bg)

                if ch.data and ch.data != ' ':
                    p.setPen(fg)
                    p.drawText(int(px), int(py + self._asc), ch.data)

        # cursor
        if is_live:
            cx, cy = self._screen.cursor.x, self._screen.cursor.y
            if 0 <= cy < self._screen.lines and 0 <= cx < self._screen.columns:
                cur = QColor(C['text'])
                cur.setAlpha(160)
                p.fillRect(int(cx * self._cw), int(cy * self._ch),
                           int(self._cw), int(self._ch), cur)

        # left accent bar
        p.fillRect(0, 0, 3, self.height(), QColor(self._accent))

        p.end()

    # ── input ──────────────────────────────────────────────────────────────

    _KEYS = {
        Qt.Key.Key_Return:   b'\r',
        Qt.Key.Key_Enter:    b'\r',
        Qt.Key.Key_Backspace:b'\x7f',
        Qt.Key.Key_Tab:      b'\t',
        Qt.Key.Key_Escape:   b'\x1b',
        Qt.Key.Key_Up:       b'\x1b[A',
        Qt.Key.Key_Down:     b'\x1b[B',
        Qt.Key.Key_Right:    b'\x1b[C',
        Qt.Key.Key_Left:     b'\x1b[D',
        Qt.Key.Key_Home:     b'\x1b[H',
        Qt.Key.Key_End:      b'\x1b[F',
        Qt.Key.Key_PageUp:   b'\x1b[5~',
        Qt.Key.Key_PageDown: b'\x1b[6~',
        Qt.Key.Key_Delete:   b'\x1b[3~',
        Qt.Key.Key_Insert:   b'\x1b[2~',
        Qt.Key.Key_F1:  b'\x1bOP',  Qt.Key.Key_F2:  b'\x1bOQ',
        Qt.Key.Key_F3:  b'\x1bOR',  Qt.Key.Key_F4:  b'\x1bOS',
        Qt.Key.Key_F5:  b'\x1b[15~',Qt.Key.Key_F6:  b'\x1b[17~',
        Qt.Key.Key_F7:  b'\x1b[18~',Qt.Key.Key_F8:  b'\x1b[19~',
        Qt.Key.Key_F9:  b'\x1b[20~',Qt.Key.Key_F10: b'\x1b[21~',
        Qt.Key.Key_F11: b'\x1b[23~',Qt.Key.Key_F12: b'\x1b[24~',
    }

    def keyPressEvent(self, event):
        mods = event.modifiers()
        key  = event.key()
        ctrl  = bool(mods & Qt.KeyboardModifier.ControlModifier)
        shift = bool(mods & Qt.KeyboardModifier.ShiftModifier)

        # Ctrl+Shift+C → copy selection (or full screen if no selection)
        if ctrl and shift and key == Qt.Key.Key_C:
            text = self._get_selected_text() or self.get_visible_text()
            QApplication.clipboard().setText(text)
            return

        # Ctrl+Shift+V → paste
        if ctrl and shift and key == Qt.Key.Key_V:
            txt = QApplication.clipboard().text()
            if txt:
                self.send_text(txt)
            return

        # Ctrl+Shift+A → select all visible + copy
        if ctrl and shift and key == Qt.Key.Key_A:
            self._select_all()
            return

        # Clear selection on any other keypress
        if self._sel_start:
            self._sel_start = None
            self._sel_end = None

        # snap to live on any keypress while scrolled
        if self._scroll_off > 0:
            self._scroll_off = 0

        if key in self._KEYS:
            self._write(self._KEYS[key])
            return

        text = event.text()
        if text:
            if ctrl and not shift and len(text) == 1:
                code = ord(text.upper()) - 64
                if 1 <= code <= 31:
                    self._write(bytes([code]))
                    return
            self._write(text.encode('utf-8'))

    def wheelEvent(self, event):
        delta = event.angleDelta().y()
        max_scroll = len(list(self._screen.history.top))
        if delta > 0:
            self._scroll_off = min(self._scroll_off + 3, max_scroll)
        else:
            self._scroll_off = max(0, self._scroll_off - 3)
        self.update()

    # ── mouse ──────────────────────────────────────────────────────────────

    def mousePressEvent(self, event):
        self.setFocus()
        if event.button() == Qt.MouseButton.MiddleButton:
            # X11 primary selection paste (middle-click convention)
            txt = QApplication.clipboard().text(QClipboard.Mode.Selection)
            if not txt:
                txt = QApplication.clipboard().text()
            if txt:
                self.send_text(txt)
            return
        if event.button() == Qt.MouseButton.LeftButton:
            row, col = self._pixel_to_cell(event.position().x(), event.position().y())
            self._sel_start = (row, col)
            self._sel_end   = (row, col)
            self.update()
        super().mousePressEvent(event)

    def mouseMoveEvent(self, event):
        if event.buttons() & Qt.MouseButton.LeftButton and self._sel_start is not None:
            row, col = self._pixel_to_cell(event.position().x(), event.position().y())
            self._sel_end = (row, col)
            self.update()

    def mouseReleaseEvent(self, event):
        if event.button() == Qt.MouseButton.LeftButton:
            text = self._get_selected_text()
            if text:
                QApplication.clipboard().setText(text)
        super().mouseReleaseEvent(event)

    def contextMenuEvent(self, event):
        menu = QMenu(self)
        paste_act    = menu.addAction('Paste  Ctrl+Shift+V')
        copy_act     = menu.addAction('Copy Selection  Ctrl+Shift+C')
        sel_all_act  = menu.addAction('Select All  Ctrl+Shift+A')
        act = menu.exec(event.globalPos())
        if act == paste_act:
            txt = QApplication.clipboard().text()
            if txt:
                self.send_text(txt)
        elif act == copy_act:
            text = self._get_selected_text() or self.get_visible_text()
            QApplication.clipboard().setText(text)
        elif act == sel_all_act:
            self._select_all()

    # ── resize ─────────────────────────────────────────────────────────────

    def resizeEvent(self, event):
        super().resizeEvent(event)
        new_cols = max(1, int(self.width()  / self._cw))
        new_rows = max(1, int(self.height() / self._ch))
        if new_cols != self._cols or new_rows != self._rows:
            self._cols, self._rows = new_cols, new_rows
            self._screen.resize(new_rows, new_cols)
            if self._master_fd is not None:
                try:
                    fcntl.ioctl(self._master_fd, termios.TIOCSWINSZ,
                                struct.pack('HHHH', new_rows, new_cols, 0, 0))
                except OSError:
                    pass
        self.update()

    def focusInEvent(self, event):
        super().focusInEvent(event)
        self.focus_gained.emit()
        self.update()

    def focusOutEvent(self, event):
        super().focusOutEvent(event)
        self.update()

    # ── drag + drop ────────────────────────────────────────────────────────

    def dragEnterEvent(self, event):
        if event.mimeData().hasUrls() or event.mimeData().hasImage():
            event.acceptProposedAction()

    def dragMoveEvent(self, event):
        event.acceptProposedAction()

    def dropEvent(self, event):
        md = event.mimeData()
        if md.hasUrls():
            paths = []
            for url in md.urls():
                path = url.toLocalFile()
                if path:
                    # If it's an image file, just paste the path
                    paths.append(f'"{path}"')
            if paths:
                self.send_text(' '.join(paths))
            event.acceptProposedAction()
        elif md.hasImage():
            _PASTE_DIR.mkdir(parents=True, exist_ok=True)
            img = QImage(md.imageData())
            if not img.isNull():
                fname = _PASTE_DIR / f'paste_{int(time.time())}.png'
                img.save(str(fname))
                self.send_text(f'"{fname}"')
                _log.info('image dropped → saved to %s', fname)
            event.acceptProposedAction()
