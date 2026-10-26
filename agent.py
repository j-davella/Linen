import json
import re
import subprocess
import threading
from pathlib import Path

from PyQt6.QtCore import QThread, pyqtSignal

import log
_log = log.get('agent')

CLAUDE_BIN = '/run/current-system/sw/bin/claude'

_SYSTEM = """\

== LINEN CO-WORK MODE ==
You are working inside Linen, a co-working shell. Rules:

1. To run any shell command, write EXACTLY:
   <RUN>command here</RUN>
   Then STOP. Do not write anything after the closing tag.

2. I will execute the command in the real terminal and reply with the output.
   After receiving "Command output:", continue your work or give the next <RUN>.

3. If a command needs authentication (sudo, SSH passphrase, etc.), write:
   "Authentication needed — please auth in the terminal below then reply 'ready'."
   Stop and wait. Do not attempt to run the command yourself.

4. Never use the built-in Bash tool. Only <RUN> tags.
5. One <RUN> block per reply, never more.\
"""

_RUN_RE = re.compile(r'<RUN>(.*?)</RUN>', re.DOTALL)


class LinenAgent(QThread):
    text_chunk    = pyqtSignal(str)
    cmd_start     = pyqtSignal(str)
    exec_request  = pyqtSignal(str)
    turn_complete = pyqtSignal()
    agent_error   = pyqtSignal(str)

    def __init__(self, parent=None):
        super().__init__(parent)
        self._session_id: str | None = None
        self._model = 'claude-sonnet-4-6'
        self._pending: list[str] = []
        self._alive = True
        self._exec_event  = threading.Event()
        self._exec_result: str | None = None

    def set_model(self, model: str):
        self._model = model
        self._session_id = None

    def send(self, text: str):
        self._pending.append(text)
        if not self.isRunning():
            self.start()

    def provide_exec_result(self, output: str):
        self._exec_result = output
        self._exec_event.set()

    def abort(self):
        self._alive = False
        self._exec_event.set()  # unblock any waiting _do_turn

    def run(self):
        self._alive = True
        while self._pending and self._alive:
            self._do_turn(self._pending.pop(0))
        self.turn_complete.emit()

    def _do_turn(self, message: str):
        if not self._alive:
            return

        _log.info('turn start | model=%s session=%s msg=%.120s',
                  self._model, self._session_id, message.replace('\n', ' '))

        cmd = [CLAUDE_BIN]
        if self._session_id:
            cmd += ['--resume', self._session_id]
        cmd += [
            '-p', message,
            '--output-format', 'stream-json',
            '--verbose',
            '--include-partial-messages',
            '--tools', '',
            '--model', self._model,
            '--append-system-prompt', _SYSTEM,
        ]

        _log.debug('claude cmd: %s', ' '.join(cmd[:8]))  # log without full system prompt

        try:
            proc = subprocess.Popen(
                cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                cwd=str(Path.home()),
            )
        except Exception as e:
            _log.exception('failed to spawn claude')
            self.agent_error.emit(str(e))
            return

        accumulated_text = ''
        last_streamed = 0
        event_count = 0

        for raw in proc.stdout:
            if not self._alive:
                _log.info('abort detected mid-stream')
                proc.terminate()
                return
            raw = raw.strip()
            if not raw:
                continue
            event_count += 1
            try:
                evt = json.loads(raw)
            except json.JSONDecodeError:
                _log.warning('json decode error: %.80s', raw)
                continue

            t = evt.get('type', '')
            _log.debug('event #%d type=%s', event_count, t)

            if t == 'result':
                self._session_id = evt.get('session_id') or self._session_id
                _log.info('result event | session=%s is_error=%s',
                          self._session_id, evt.get('is_error'))
                if evt.get('is_error') or evt.get('api_error_status'):
                    err = str(evt.get('api_error_status') or 'API error')
                    _log.error('api error: %s', err)
                    self.agent_error.emit(err)

            elif t == 'assistant':
                for block in evt.get('message', {}).get('content', []):
                    if block.get('type') == 'text':
                        full_text = block.get('text', '')
                        if len(full_text) > last_streamed:
                            new_part = full_text[last_streamed:]
                            last_streamed = len(full_text)
                            accumulated_text = full_text
                            _log.debug('text chunk +%d chars (total %d)',
                                       len(new_part), len(full_text))
                            visible = _RUN_RE.sub('', new_part)
                            if visible.strip():
                                self.text_chunk.emit(visible)

        proc.wait()
        _log.info('claude exited rc=%d events=%d text_len=%d',
                  proc.returncode, event_count, len(accumulated_text))

        if proc.returncode != 0:
            err = proc.stderr.read().strip()
            _log.error('stderr: %s', err[:500])
            if err:
                self.agent_error.emit(err)
            return

        if not self._alive:
            return

        m = _RUN_RE.search(accumulated_text)
        if m:
            run_cmd = m.group(1).strip()
            _log.info('RUN tag found: %.120s', run_cmd)
            self.cmd_start.emit(run_cmd)
            self.exec_request.emit(run_cmd)

            self._exec_event.clear()
            self._exec_result = None
            _log.info('waiting for exec result (timeout=120s)')
            fired = self._exec_event.wait(timeout=120)
            _log.info('exec event fired=%s alive=%s result_len=%s',
                      fired, self._alive,
                      len(self._exec_result) if self._exec_result else 0)

            if not self._alive:
                return

            output = self._exec_result or '[command timed out — type Ctrl+C in terminal if it is still running]'
            self._do_turn(f'Command output:\n```\n{output}\n```\n\nPlease continue.')
        else:
            _log.info('no RUN tag — turn complete')
