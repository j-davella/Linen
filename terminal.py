import re
import sys
import os
import pty
import fcntl
import termios
import struct
import select
import subprocess
from pathlib import Path

import log
_log = log.get('terminal')

_ANSI_RE = re.compile(
    r'\x1b(?:[@-Z\\-_]|\[[0-9;]*[A-Za-z]|\][^\x07]*\x07)'
)

_USER_PKGS = Path.home() / '.local' / 'lib' / 'python3.13' / 'site-packages'
if str(_USER_PKGS) not in sys.path and _USER_PKGS.exists():
    sys.path.insert(0, str(_USER_PKGS))

import pyte

from PyQt6.QtWidgets import QWidget, QApplication
from PyQt6.QtGui import QPainter, QFont, QFontMetricsF, QColor
from PyQt6.QtCore import Qt, QThread, pyqtSignal, QTimer

from theme import resolve_color, C
from config import FONT_FAMILY, FONT_SIZE, SCROLLBACK


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

        # output capture (used by LinenWindow to feed results back to agent)
        self._cap_active   = False
        self._cap_sentinel = ''
        self._cap_buf: list[str] = []

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
        # Respond to terminal capability queries so Claude Code renders properly
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

    def start_capture(self, sentinel: str):
        self._cap_buf = []
        self._cap_sentinel = sentinel
        self._cap_active = True
        _log.debug('capture started sentinel=%s', sentinel)

    def stop_capture(self):
        self._cap_active = False
        raw_len = sum(len(b) for b in self._cap_buf)
        _log.debug('capture stopped raw_bytes=%d', raw_len)

    def get_capture(self) -> str:
        raw = ''.join(self._cap_buf)
        return _ANSI_RE.sub('', raw).replace('\r\n', '\n').replace('\r', '\n')

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

        # Ctrl+Shift+C → copy screen text
        if ctrl and shift and key == Qt.Key.Key_C:
            QApplication.clipboard().setText(self.get_visible_text())
            return

        # Ctrl+Shift+V → paste
        if ctrl and shift and key == Qt.Key.Key_V:
            txt = QApplication.clipboard().text()
            if txt:
                self.send_text(txt)
            return

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

    def mousePressEvent(self, event):
        self.setFocus()
        super().mousePressEvent(event)

    def focusInEvent(self, event):
        super().focusInEvent(event)
        self.focus_gained.emit()
        self.update()

    def focusOutEvent(self, event):
        super().focusOutEvent(event)
        self.update()
