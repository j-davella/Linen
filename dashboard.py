import json
import time
import uuid
from pathlib import Path

from PyQt6.QtCore import QObject, QThread, QTimer, QUrl, pyqtSignal, pyqtSlot
from PyQt6.QtWebChannel import QWebChannel
from PyQt6.QtWebEngineCore import QWebEngineScript
from PyQt6.QtWebEngineWidgets import QWebEngineView
from PyQt6.QtWidgets import QMainWindow

import audit as _audit
import log
_log = log.get('dashboard')

from agent import LinenAgent
from config import DATA_DIR, BASH_CMD, DEFAULT_MODEL
from terminal import TerminalWidget

_HTML = Path(__file__).parent / 'dashboard.html'

# Patterns that mean the PTY is waiting for the user to type a password
_AUTH_PATTERNS = (
    '[sudo] password',
    'password for',
    'enter passphrase',
    'enter password',
    'password:',
    '(current) unix password',
)


# ── background job runner ─────────────────────────────────────────────────────

class BgJobThread(QThread):
    progress = pyqtSignal(str, str)   # job_id, line
    done     = pyqtSignal(str, int, str)  # job_id, exit_code, output

    def __init__(self, job_id: str, cmd: str, parent=None):
        super().__init__(parent)
        self._job_id = job_id
        self._cmd = cmd

    def run(self):
        import subprocess
        try:
            proc = subprocess.Popen(
                ['bash', '-c', self._cmd],
                stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                text=True, cwd=str(Path.home()),
            )
            lines = []
            for line in proc.stdout:
                lines.append(line)
                self.progress.emit(self._job_id, line.rstrip())
            proc.wait()
            self.done.emit(self._job_id, proc.returncode, ''.join(lines))
        except Exception as e:
            self.done.emit(self._job_id, 1, str(e))


# ── WebChannel bridge ─────────────────────────────────────────────────────────

class LinenBridge(QObject):
    # Python → JS
    update      = pyqtSignal(str)
    feedLine    = pyqtSignal(str)
    streamStart = pyqtSignal()
    streamChunk = pyqtSignal(str)
    streamEnd   = pyqtSignal()
    setBusy     = pyqtSignal(bool)

    def __init__(self, parent=None):
        super().__init__(parent)
        self._on_send     = None
        self._on_terminal = None

    def set_send_callback(self, fn):     self._on_send = fn
    def set_terminal_callback(self, fn): self._on_terminal = fn

    @pyqtSlot(str)
    def sendMessage(self, text: str):
        if self._on_send and text.strip():
            self._on_send(text.strip())

    @pyqtSlot(str)
    def sendToTerminal(self, cmd: str):
        if self._on_terminal and cmd.strip():
            self._on_terminal(cmd.strip())


# ── main dashboard window ─────────────────────────────────────────────────────

class DashboardWindow(QMainWindow):
    def __init__(self):
        super().__init__()
        self._start       = time.time()
        self._tool_count  = 0
        self._cmd_count   = 0
        self._stream_buf  = ''
        self._poll_timer: QTimer | None = None
        self._poll_elapsed = 0
        self._bg_jobs: dict[str, BgJobThread] = {}

        DATA_DIR.mkdir(parents=True, exist_ok=True)

        # Hidden PTY for <RUN> execution
        self._term = TerminalWidget(BASH_CMD)
        self._term.resize(960, 480)
        self._term.hide()

        self._audit = _audit.Session(DEFAULT_MODEL)

        self._agent = LinenAgent()
        self._agent.set_audit(self._audit)
        self._agent.text_chunk.connect(self._on_text)
        self._agent.cmd_start.connect(self._on_cmd_start)
        self._agent.exec_request.connect(self._on_exec_request)
        self._agent.bg_request.connect(self._on_bg_request)
        self._agent.turn_complete.connect(self._on_turn_done)
        self._agent.agent_error.connect(self._on_error)

        # Throttle stream chunks to 80 ms windows
        self._flush_timer = QTimer(self)
        self._flush_timer.setInterval(80)
        self._flush_timer.timeout.connect(self._flush_stream)

        self._bridge = LinenBridge(parent=self)
        self._bridge.set_send_callback(self._user_send)
        self._bridge.set_terminal_callback(self._term_send)

        self._channel = QWebChannel(self)
        self._channel.registerObject('bridge', self._bridge)

        self._view = QWebEngineView(self)
        self._view.page().setWebChannel(self._channel)
        self._inject_qwebchannel()
        self._view.setUrl(QUrl.fromLocalFile(str(_HTML)))
        self.setCentralWidget(self._view)

        self.setWindowTitle('Linen')
        self.resize(1600, 1000)

        self._tick = QTimer(self)
        self._tick.setInterval(1000)
        self._tick.timeout.connect(self._push_stats)
        self._tick.start()

    def _inject_qwebchannel(self):
        s = QWebEngineScript()
        s.setName('qwebchannel_js')
        s.setSourceUrl(QUrl('qrc:///qtwebchannel/qwebchannel.js'))
        s.setInjectionPoint(QWebEngineScript.InjectionPoint.DocumentCreation)
        s.setWorldId(QWebEngineScript.ScriptWorldId.MainWorld)
        self._view.page().scripts().insert(s)

    # ── terminal direct input ─────────────────────────────────────────────

    def _term_send(self, cmd: str):
        _log.info('term_send: %.120s', cmd)
        self._term.send_text(cmd + '\n')
        # pick up last line of visible output after a short delay
        QTimer.singleShot(800, self._feed_term_visible)

    def _feed_term_visible(self):
        lines = [l for l in self._term.get_visible_text().splitlines() if l.strip()]
        if lines:
            self._bridge.feedLine.emit(json.dumps({
                'pane': 'bot', 'ev': 'log',
                'prefix': '<span class="g">out</span> ',
                'text': lines[-1][:80],
            }))

    # ── user sends a Claude message ───────────────────────────────────────

    def _user_send(self, text: str):
        _log.info('user_send: %.120s', text)
        self._bridge.setBusy.emit(True)
        self._bridge.streamStart.emit()
        self._flush_timer.start()
        self._agent.send(text)

    # ── agent signal handlers ─────────────────────────────────────────────

    def _on_text(self, chunk: str):
        self._stream_buf += chunk

    def _flush_stream(self):
        if self._stream_buf:
            self._bridge.streamChunk.emit(self._stream_buf)
            self._stream_buf = ''

    def _on_cmd_start(self, cmd: str):
        self._tool_count += 1
        self._bridge.feedLine.emit(json.dumps({
            'pane': 'top', 'ev': 'tool',
            'prefix': '<span class="p">claude</span> ',
            'text': cmd[:80],
        }))

    def _on_exec_request(self, cmd: str, timeout_s: int):
        self._cmd_count += 1
        _log.info('exec_request timeout=%ds: %.120s', timeout_s, cmd)
        self._bridge.feedLine.emit(json.dumps({
            'pane': 'bot', 'ev': 'shell',
            'prefix': '<span class="c">$</span> ',
            'text': cmd[:80],
        }))

        sentinel = f'LINEN_{uuid.uuid4().hex[:16]}'

        # Wrap in bash to avoid login-shell quoting issues (fish, etc.)
        import tempfile, os as _os
        tmpdir = Path(tempfile.mkdtemp(prefix='linen_'))
        script = tmpdir / 'cmd.sh'
        script.write_text(f'#!/usr/bin/env bash\n{cmd}\n')
        script.chmod(0o755)
        full_cmd = f'bash {script}; _rc=$?; rm -rf {tmpdir}; printf "\\n{sentinel}:$_rc\\n"'

        self._term.start_capture(sentinel)
        self._term.send_text(full_cmd + '\n')

        if self._poll_timer and self._poll_timer.isActive():
            self._poll_timer.stop()

        self._poll_timer = QTimer(self)
        self._poll_timer.setInterval(200)
        self._poll_elapsed = 0

        _shown_offset   = [0]
        _auth_surfaced  = [False]
        _last_out_len   = [0]
        _last_out_time  = [time.monotonic()]

        def _poll():
            self._poll_elapsed += 200
            captured = self._term.get_capture()

            # ── live output streaming ──────────────────────────────────
            new_part = captured[_shown_offset[0]:]
            if new_part:
                for line in new_part.split('\n')[:-1]:
                    line = line.strip()
                    if line and sentinel not in line:
                        self._bridge.feedLine.emit(json.dumps({
                            'pane': 'bot', 'ev': 'log',
                            'prefix': '<span class="g">out</span> ',
                            'text': line[:80],
                        }))
                _shown_offset[0] = max(0, len(captured) - len(new_part.split('\n')[-1]))

            # ── activity tracker (for auto-extend) ─────────────────────
            if len(captured) > _last_out_len[0]:
                _last_out_len[0] = len(captured)
                _last_out_time[0] = time.monotonic()

            # ── auth detection ─────────────────────────────────────────
            if not _auth_surfaced[0]:
                low = captured.lower()
                if any(p in low for p in _AUTH_PATTERNS):
                    _auth_surfaced[0] = True
                    self._bridge.feedLine.emit(json.dumps({
                        'pane': 'top', 'ev': 'log',
                        'prefix': '<span class="y">auth</span> ',
                        'text': 'Password required — type it in the shell bar below',
                    }))
                    _log.info('auth prompt detected, surfaced to user')
                    # keep waiting — user types password in terminal bar

            # ── sentinel check ─────────────────────────────────────────
            marker = sentinel + ':'
            idx = captured.rfind(marker)
            if idx >= 0:
                self._poll_timer.stop()
                self._term.stop_capture()
                before = captured[:idx]
                nl = before.find('\n')
                output = before[nl + 1:].strip() if nl >= 0 else before.strip()
                _log.info('sentinel found after %dms output_len=%d', self._poll_elapsed, len(output))
                self._agent.provide_exec_result(output or '[no output]')
                return

            # ── timeout with auto-extend if output is still flowing ────
            if self._poll_elapsed >= timeout_s * 1000:
                idle_s = time.monotonic() - _last_out_time[0]
                # extend up to 2× the requested timeout while output flows
                if idle_s < 15 and self._poll_elapsed < timeout_s * 2000:
                    return  # still alive — keep waiting
                self._poll_timer.stop()
                self._term.stop_capture()
                _log.warning('exec timeout after %dms cmd=%.80s', self._poll_elapsed, cmd)
                self._agent.provide_exec_result(
                    f'[timed out after {self._poll_elapsed // 1000}s — '
                    f'command may still be running; use the shell bar to interact]'
                )

        self._poll_timer.timeout.connect(_poll)
        self._poll_timer.start()

    # ── background jobs ───────────────────────────────────────────────────

    def _on_bg_request(self, job_id: str, cmd: str):
        _log.info('bg_request id=%s cmd=%.120s', job_id, cmd)
        self._bridge.feedLine.emit(json.dumps({
            'pane': 'bot', 'ev': 'shell',
            'prefix': f'<span class="g">[{job_id}]</span> ',
            'text': cmd[:80],
        }))
        thread = BgJobThread(job_id, cmd, parent=self)
        thread.progress.connect(self._on_bg_progress)
        thread.done.connect(self._on_bg_done)
        thread.start()
        self._bg_jobs[job_id] = thread

    def _on_bg_progress(self, job_id: str, line: str):
        if line.strip():
            self._bridge.feedLine.emit(json.dumps({
                'pane': 'bot', 'ev': 'log',
                'prefix': f'<span class="g">[{job_id[:8]}]</span> ',
                'text': line[:80],
            }))

    def _on_bg_done(self, job_id: str, exit_code: int, output: str):
        _log.info('bg job done id=%s exit=%d output_len=%d', job_id, exit_code, len(output))
        self._bg_jobs.pop(job_id, None)
        self._bridge.feedLine.emit(json.dumps({
            'pane': 'bot', 'ev': 'tool',
            'prefix': f'<span class="g">[{job_id[:8]}]</span> ',
            'text': f'done (exit {exit_code})',
        }))
        # inject result as a new message so Claude can continue
        tail = output[-4000:] if len(output) > 4000 else output
        self._agent.send(
            f"Background job '{job_id}' completed (exit code {exit_code}).\n"
            f"Output:\n```\n{tail}\n```\nPlease continue."
        )

    # ── turn lifecycle ────────────────────────────────────────────────────

    def _on_turn_done(self):
        self._flush_timer.stop()
        self._flush_stream()
        self._bridge.streamEnd.emit()
        self._bridge.setBusy.emit(False)

    def _on_error(self, msg: str):
        self._flush_timer.stop()
        self._stream_buf = ''
        self._bridge.streamEnd.emit()
        self._bridge.setBusy.emit(False)
        self._bridge.feedLine.emit(json.dumps({
            'pane': 'top', 'ev': 'log',
            'prefix': '<span class="y">err</span> ',
            'text': msg[:80],
        }))

    # ── stats push ────────────────────────────────────────────────────────

    def _push_stats(self):
        db = DATA_DIR / 'audit.db'
        logs_mb = round(db.stat().st_size / 1_000_000, 1) if db.exists() else 0.0
        active_bg = len(self._bg_jobs)
        self._bridge.update.emit(json.dumps({
            'tools': self._tool_count,
            'cmds':  self._cmd_count,
            'logsMB': logs_mb,
            'uptimeMs': int((time.time() - self._start) * 1000),
            'instances': [{
                'name':   'current session',
                'host':   'device',
                'status': 'ACTIVE' if (self._agent.isRunning() or active_bg) else 'IDLE',
                'logs':   self._tool_count + self._cmd_count,
                'seg':    min(16, (self._tool_count + self._cmd_count) // 3),
            }],
        }))

    # ── lifecycle ─────────────────────────────────────────────────────────

    def closeEvent(self, event):
        self._agent.abort()
        self._agent.wait(2000)
        for t in self._bg_jobs.values():
            t.terminate()
            t.wait(1000)
        if self._poll_timer and self._poll_timer.isActive():
            self._poll_timer.stop()
        self._term.stop_capture()
        self._term.shutdown()
        self._audit.close()
        super().closeEvent(event)
