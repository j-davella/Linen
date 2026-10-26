import sys
import uuid
import html as _html

import log
_log = log.get('main')

from PyQt6.QtWidgets import (
    QApplication, QMainWindow, QWidget, QVBoxLayout, QHBoxLayout,
    QSplitter, QLabel, QComboBox, QPushButton, QTextEdit, QLineEdit,
)
from PyQt6.QtCore import Qt, QTimer
from PyQt6.QtGui import QFont, QPainter, QColor, QTextCursor, QTextCharFormat

from theme import C, build_stylesheet
from config import MODELS, DEFAULT_MODEL, DATA_DIR, BASH_CMD
from agent import LinenAgent
from terminal import TerminalWidget


def _e(text: str) -> str:
    return _html.escape(str(text))


# ── chat panel (top) ───────────────────────────────────────────────────────────

class ChatPanel(QWidget):
    def __init__(self, parent=None):
        super().__init__(parent)
        vbox = QVBoxLayout(self)
        vbox.setContentsMargins(0, 0, 0, 0)
        vbox.setSpacing(0)

        self._history = QTextEdit()
        self._history.setReadOnly(True)
        self._history.setFont(QFont('JetBrainsMono Nerd Font', 11))
        self._history.setStyleSheet(f"""
            QTextEdit {{
                background: {C['base']};
                color: {C['text']};
                border: none;
                padding: 14px 16px;
                line-height: 1.5;
            }}
        """)
        vbox.addWidget(self._history)

        bar = QWidget()
        bar.setFixedHeight(52)
        blay = QHBoxLayout(bar)
        blay.setContentsMargins(10, 8, 10, 8)
        blay.setSpacing(8)

        self.input = QLineEdit()
        self.input.setPlaceholderText('Message Linen…')
        self.input.setFont(QFont('JetBrainsMono Nerd Font', 11))
        self.input.setStyleSheet(f"""
            QLineEdit {{
                background: {C['surface0']};
                color: {C['text']};
                border: 1px solid {C['surface2']};
                border-radius: 5px;
                padding: 7px 12px;
            }}
            QLineEdit:focus {{ border-color: {C['mauve']}; }}
            QLineEdit:disabled {{ color: {C['overlay0']}; }}
        """)
        blay.addWidget(self.input)

        self._send_btn = QPushButton('Send')
        self._send_btn.setFixedSize(70, 34)
        self._send_btn.setStyleSheet(f"""
            QPushButton {{
                background: {C['mauve']};
                color: {C['crust']};
                border: none; border-radius: 5px;
                font-weight: bold;
                font-family: 'JetBrainsMono Nerd Font';
                font-size: 11px;
            }}
            QPushButton:hover {{ background: {C['lavender']}; }}
            QPushButton:disabled {{ background: {C['surface1']}; color: {C['overlay0']}; }}
        """)
        blay.addWidget(self._send_btn)

        self._stop_btn = QPushButton('Stop')
        self._stop_btn.setFixedSize(70, 34)
        self._stop_btn.setVisible(False)
        self._stop_btn.setStyleSheet(f"""
            QPushButton {{
                background: {C['red']};
                color: {C['crust']};
                border: none; border-radius: 5px;
                font-weight: bold;
                font-family: 'JetBrainsMono Nerd Font';
                font-size: 11px;
            }}
            QPushButton:hover {{ background: {C['peach']}; }}
        """)
        blay.addWidget(self._stop_btn)

        bar.setStyleSheet(f"""
            QWidget {{
                background: {C['mantle']};
                border-top: 2px solid {C['surface0']};
            }}
        """)
        vbox.addWidget(bar)

        self._on_send = None
        self._on_stop = None
        self._send_btn.clicked.connect(self._submit)
        self.input.returnPressed.connect(self._submit)
        self._stop_btn.clicked.connect(self._do_stop)

    def on_send(self, fn):
        self._on_send = fn

    def on_stop(self, fn):
        self._on_stop = fn

    def set_busy(self, busy: bool):
        self.input.setEnabled(not busy)
        self._send_btn.setVisible(not busy)
        self._stop_btn.setVisible(busy)
        if not busy:
            self.input.setFocus()

    def _submit(self):
        text = self.input.text().strip()
        if not text or not self._on_send:
            return
        self.input.clear()
        self._on_send(text)

    def _do_stop(self):
        if self._on_stop:
            self._on_stop()

    def add_user(self, text: str):
        self._history.append(
            f'<p style="margin:14px 0 4px 0; color:{C["mauve"]}; font-weight:bold;">You</p>'
            f'<p style="margin:0; color:{C["text"]}; white-space:pre-wrap;">{_e(text)}</p>'
        )
        self._snap()

    def begin_assistant(self):
        self._history.append(
            f'<p style="margin:14px 0 4px 0; color:{C["teal"]}; font-weight:bold;">Linen</p>'
        )
        self._snap()

    def stream_text(self, chunk: str):
        cur = self._history.textCursor()
        cur.movePosition(QTextCursor.MoveOperation.End)
        fmt = QTextCharFormat()
        fmt.setForeground(QColor(C['text']))
        cur.insertText(chunk, fmt)
        self._history.setTextCursor(cur)
        self._snap()

    def end_assistant(self):
        self._history.append('<p></p>')
        self._snap()

    def add_tool_chip(self, cmd: str):
        self._history.append(
            f'<p style="margin:6px 0; padding:4px 8px; '
            f'background:{C["surface0"]}; border-left:3px solid {C["yellow"]}; '
            f'color:{C["yellow"]}; font-family:monospace;">▶ {_e(cmd)}</p>'
        )
        self._snap()

    def _snap(self):
        sb = self._history.verticalScrollBar()
        sb.setValue(sb.maximum())


# ── drag handle + header ───────────────────────────────────────────────────────

class DragHandle(QWidget):
    def __init__(self, parent=None):
        super().__init__(parent)
        self.setCursor(Qt.CursorShape.SizeAllCursor)
        self.setFixedWidth(180)
        lay = QHBoxLayout(self)
        lay.setContentsMargins(14, 0, 10, 0)
        lay.setSpacing(8)
        grip = QLabel('⠿')
        grip.setStyleSheet(f"color:{C['surface2']}; font-size:16px;")
        lay.addWidget(grip)
        lbl = QLabel('Linen')
        lbl.setStyleSheet(
            f"color:{C['mauve']}; font-family:'JetBrainsMono Nerd Font'; "
            f"font-size:14px; font-weight:bold; letter-spacing:3px;"
        )
        lay.addWidget(lbl)

    def mousePressEvent(self, e):
        if e.button() == Qt.MouseButton.LeftButton:
            self.window().windowHandle().startSystemMove()

    def mouseDoubleClickEvent(self, _e):
        w = self.window()
        w.showNormal() if w.isMaximized() else w.showMaximized()


class HeaderBar(QWidget):
    def __init__(self, parent=None):
        super().__init__(parent)
        self.setFixedHeight(44)
        lay = QHBoxLayout(self)
        lay.setContentsMargins(0, 0, 10, 0)
        lay.setSpacing(10)

        lay.addWidget(DragHandle())

        spacer = QWidget()
        spacer.setAttribute(Qt.WidgetAttribute.WA_TransparentForMouseEvents)
        lay.addWidget(spacer, stretch=1)

        lbl = QLabel('model')
        lbl.setStyleSheet(f"color:{C['overlay0']}; font-size:10px;")
        lay.addWidget(lbl)

        self.model_combo = QComboBox()
        self.model_combo.addItems(MODELS)
        self.model_combo.setCurrentText(DEFAULT_MODEL)
        self.model_combo.setFixedWidth(230)
        lay.addWidget(self.model_combo)

        close_btn = QPushButton('✕')
        close_btn.setFixedSize(28, 28)
        close_btn.setStyleSheet(f"""
            QPushButton {{
                background:transparent; color:{C['overlay0']};
                border:none; font-size:14px; font-weight:bold;
            }}
            QPushButton:hover {{
                color:{C['red']}; background:{C['surface0']}; border-radius:4px;
            }}
        """)
        close_btn.clicked.connect(lambda: self.window().close())
        lay.addWidget(close_btn)

    def paintEvent(self, _):
        p = QPainter(self)
        p.fillRect(self.rect(), QColor(C['mantle']))
        p.fillRect(0, self.height() - 2, self.width(), 2, QColor(C['surface0']))
        p.end()

    def mousePressEvent(self, e):
        if e.button() == Qt.MouseButton.LeftButton:
            self.window().windowHandle().startSystemMove()


# ── main window ────────────────────────────────────────────────────────────────

class LinenWindow(QMainWindow):
    def __init__(self):
        super().__init__()
        self.setWindowTitle('Linen')
        self.resize(1400, 900)
        self.setWindowFlags(Qt.WindowType.FramelessWindowHint)
        DATA_DIR.mkdir(parents=True, exist_ok=True)

        self._poll_timer: QTimer | None = None

        self._agent = LinenAgent()
        self._agent.text_chunk.connect(self._on_text)
        self._agent.cmd_start.connect(self._on_cmd_start)
        self._agent.exec_request.connect(self._on_exec_request)
        self._agent.turn_complete.connect(self._on_turn_done)
        self._agent.agent_error.connect(self._on_error)

        self._build_ui()

    def _build_ui(self):
        root = QWidget()
        root.setStyleSheet(f"border: 1px solid {C['surface1']};")
        self.setCentralWidget(root)

        vbox = QVBoxLayout(root)
        vbox.setContentsMargins(1, 1, 1, 1)
        vbox.setSpacing(0)

        self._header = HeaderBar()
        self._header.model_combo.currentTextChanged.connect(self._agent.set_model)
        vbox.addWidget(self._header)

        inner = QWidget()
        inner.setStyleSheet('QWidget { border: none; }')
        ibox = QVBoxLayout(inner)
        ibox.setContentsMargins(0, 0, 0, 0)
        ibox.setSpacing(0)

        splitter = QSplitter(Qt.Orientation.Vertical)
        splitter.setHandleWidth(5)

        self._chat = ChatPanel()
        self._chat.on_send(self._user_send)
        self._chat.on_stop(self._user_stop)

        self._term = TerminalWidget(BASH_CMD, accent_color=C['teal'])

        splitter.addWidget(self._chat)
        splitter.addWidget(self._term)
        splitter.setSizes([460, 400])
        ibox.addWidget(splitter)
        vbox.addWidget(inner)

        QTimer.singleShot(100, self._chat.input.setFocus)

    # ── routing ────────────────────────────────────────────────────────────

    def _user_send(self, text: str):
        self._chat.add_user(text)
        self._chat.set_busy(True)
        self._chat.begin_assistant()
        self._agent.send(text)

    def _user_stop(self):
        self._agent.abort()
        if self._poll_timer and self._poll_timer.isActive():
            self._poll_timer.stop()
        self._term.stop_capture()
        self._chat.stream_text('\n[stopped]')
        self._chat.end_assistant()
        self._chat.set_busy(False)

    # ── agent signal handlers ──────────────────────────────────────────────

    def _on_text(self, chunk: str):
        self._chat.stream_text(chunk)

    def _on_cmd_start(self, cmd: str):
        self._chat.add_tool_chip(cmd)

    def _on_exec_request(self, cmd: str):
        _log.info('exec_request: %.120s', cmd)
        sentinel = f'LINEN_{uuid.uuid4().hex[:16]}'
        self._term.start_capture(sentinel)
        self._term.send_text(f'{cmd}; echo "{sentinel}:$?"\n')

        if self._poll_timer and self._poll_timer.isActive():
            self._poll_timer.stop()

        self._poll_timer = QTimer()
        self._poll_timer.setInterval(200)
        self._poll_elapsed = 0

        def _poll():
            self._poll_elapsed += 200
            text = self._term.get_capture()
            marker = sentinel + ':'
            # rfind: sentinel appears in the echoed command line too, use last occurrence
            idx = text.rfind(marker)
            if idx >= 0:
                self._poll_timer.stop()
                self._term.stop_capture()
                before = text[:idx]
                nl = before.find('\n')
                output = before[nl + 1:].strip() if nl >= 0 else before.strip()
                _log.info('sentinel found after %dms output_len=%d output=%.200s',
                          self._poll_elapsed, len(output), output.replace('\n', '↵'))
                self._agent.provide_exec_result(output or '[no output]')
            elif self._poll_elapsed >= 90_000:  # 90s client-side timeout
                self._poll_timer.stop()
                self._term.stop_capture()
                _log.warning('exec timeout after 90s for cmd: %.120s', cmd)
                self._agent.provide_exec_result(
                    '[timed out after 90s — command may still be running in terminal]'
                )

        self._poll_timer.timeout.connect(_poll)
        self._poll_timer.start()

    def _on_turn_done(self):
        self._chat.end_assistant()
        self._chat.set_busy(False)

    def _on_error(self, msg: str):
        self._chat.stream_text(f'\n[error: {msg}]')
        self._chat.end_assistant()
        self._chat.set_busy(False)

    def closeEvent(self, event):
        self._agent.abort()
        self._agent.wait(2000)
        self._term.shutdown()
        super().closeEvent(event)


def main():
    app = QApplication(sys.argv)
    app.setApplicationName('Linen')
    app.setStyleSheet(build_stylesheet())
    win = LinenWindow()
    win.show()
    sys.exit(app.exec())


if __name__ == '__main__':
    main()
