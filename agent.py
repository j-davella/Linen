import json
import re
import subprocess
import threading
import time
from pathlib import Path

from PyQt6.QtCore import QThread, pyqtSignal

import audit as _audit
import log
_log = log.get('agent')

CLAUDE_BIN = '/run/current-system/sw/bin/claude'
_CONTEXT_FILE = Path.home() / '.linen' / 'context.md'


def _build_system_prompt() -> str:
    context = ''
    if _CONTEXT_FILE.exists():
        try:
            context = _CONTEXT_FILE.read_text().strip()
        except OSError:
            pass
    base = _SYSTEM_BASE
    if context:
        return f'== LINEN CONTEXT ==\n{context}\n\n{base}'
    return base


_SYSTEM_BASE = """\

== LINEN CO-WORK MODE ==
You are working inside Linen, a co-working shell. Always use bash/POSIX syntax.

AVAILABLE ACTIONS (one per reply, then STOP):

1. Run a shell command:
   <RUN>command here</RUN>
   Linen executes it and returns "Command output:" — then continue.

2. Long-running command (builds, installs, etc.) — set an explicit timeout in seconds:
   <RUN timeout="600">nixos-rebuild switch</RUN>
   Default is 90s. Set higher for cargo build, nix develop, etc.

3. Background job — fire-and-forget, does NOT block:
   <RUN bg="true" id="mybuild">cargo build --release</RUN>
   You'll be notified automatically with the full output when it finishes.
   Use for anything that takes more than a minute and doesn't need intermediate results.

4. Write a file directly (avoids all shell quoting issues):
   <WRITE path="/absolute/path/or/~/relative">
   file content here, verbatim, any characters
   </WRITE>
   The file is written by Python — no shell involved. Prefer this over heredocs.

AUTH: If sudo or ssh asks for a password, Linen detects it and alerts the user.
      You will receive "[AUTH REQUIRED]" — just wait, the result will come back automatically.

RULES:
- Never use the built-in Bash tool.
- One <RUN> or <WRITE> block per reply, never more.\
"""

_SYSTEM = _build_system_prompt()

_RUN_RE   = re.compile(r'<RUN\b([^>]*)>(.*?)</RUN>',     re.DOTALL)
_WRITE_RE = re.compile(r'<WRITE\b([^>]*)>(.*?)</WRITE>',  re.DOTALL)
_ATTR_RE  = re.compile(r'(\w+)=["\']([^"\']*)["\']')


def _parse_attrs(s: str) -> dict[str, str]:
    return {m.group(1): m.group(2) for m in _ATTR_RE.finditer(s)}


class LinenAgent(QThread):
    text_chunk    = pyqtSignal(str)
    cmd_start     = pyqtSignal(str)
    exec_request  = pyqtSignal(str, int)  # cmd, timeout_s
    bg_request    = pyqtSignal(str, str)  # job_id, cmd
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
        self._audit: _audit.Session | None = None

    def set_audit(self, session: _audit.Session):
        self._audit = session

    def set_model(self, model: str):
        self._model = model
        self._session_id = None
        if self._audit:
            self._audit.update_model(model)

    def send(self, text: str):
        self._pending.append(text)
        if not self.isRunning():
            self.start()

    def provide_exec_result(self, output: str):
        self._exec_result = output
        self._exec_event.set()

    def abort(self):
        self._alive = False
        self._exec_event.set()

    def run(self):
        self._alive = True
        while self._pending and self._alive:
            self._do_turn(self._pending.pop(0))
        self.turn_complete.emit()

    def _do_turn(self, message: str, _is_followup: bool = False):
        if not self._alive:
            return

        if not _is_followup and self._audit:
            self._audit.log_turn('user', message)

        _log.info('turn start | model=%s session=%s followup=%s msg=%.120s',
                  self._model, self._session_id, _is_followup,
                  message.replace('\n', ' '))

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

        _log.debug('claude cmd: %s', ' '.join(cmd[:8]))

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
            err = str(e)
            if self._audit:
                self._audit.log_error(f'spawn failed: {err}')
            self.agent_error.emit(err)
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
                sid = evt.get('session_id') or self._session_id
                if sid and sid != self._session_id:
                    self._session_id = sid
                    if self._audit:
                        self._audit.update_claude_session(sid)
                _log.info('result event | session=%s is_error=%s',
                          self._session_id, evt.get('is_error'))
                if evt.get('is_error') or evt.get('api_error_status'):
                    err = str(evt.get('api_error_status') or 'API error')
                    _log.error('api error: %s', err)
                    if self._audit:
                        self._audit.log_error(err)
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
                            visible = _WRITE_RE.sub('', visible)
                            if visible.strip():
                                self.text_chunk.emit(visible)

        proc.wait()
        _log.info('claude exited rc=%d events=%d text_len=%d',
                  proc.returncode, event_count, len(accumulated_text))

        if proc.returncode != 0:
            err = proc.stderr.read().strip()
            _log.error('stderr: %s', err[:500])
            if err:
                if self._audit:
                    self._audit.log_error(f'claude rc={proc.returncode}: {err[:500]}')
                self.agent_error.emit(err)
            return

        if not self._alive:
            return

        # Persist the full assistant response before acting on it
        if accumulated_text and self._audit:
            self._audit.log_turn('assistant', accumulated_text)

        # ── <WRITE> tag ────────────────────────────────────────────────────
        w = _WRITE_RE.search(accumulated_text)
        if w:
            attrs   = _parse_attrs(w.group(1))
            path_s  = attrs.get('path', '').strip()
            content = w.group(2)
            if content.startswith('\n'):
                content = content[1:]
            _log.info('WRITE tag path=%s len=%d', path_s, len(content))
            if path_s:
                try:
                    p = Path(path_s).expanduser()
                    p.parent.mkdir(parents=True, exist_ok=True)
                    p.write_text(content)
                    result = f'File written: {path_s} ({len(content)} bytes)'
                except Exception as e:
                    result = f'Write failed: {e}'
            else:
                result = 'Write failed: missing path= attribute'
            if self._audit:
                self._audit.log_command_start(f'WRITE {path_s}')
                self._audit.log_command_done(result)
            self._do_turn(f'Write result: {result}', _is_followup=True)
            return

        # ── <RUN> tag ──────────────────────────────────────────────────────
        m = _RUN_RE.search(accumulated_text)
        if m:
            attrs   = _parse_attrs(m.group(1))
            run_cmd = m.group(2).strip()

            # background job
            if attrs.get('bg') in ('true', '1', 'yes'):
                job_id = attrs.get('id') or f'job_{int(time.time())}'
                _log.info('BG job id=%s cmd=%.120s', job_id, run_cmd)
                self.cmd_start.emit(f'[bg:{job_id}] {run_cmd}')
                if self._audit:
                    self._audit.log_command_start(f'BG:{job_id} {run_cmd}')
                self.bg_request.emit(job_id, run_cmd)
                self._do_turn(
                    f"Background job '{job_id}' started.\n"
                    f"I'll be notified automatically when it completes.",
                    _is_followup=True,
                )
                return

            # regular (possibly long) run
            timeout_s = max(5, int(attrs.get('timeout', '90')))
            _log.info('RUN tag timeout=%ds: %.120s', timeout_s, run_cmd)
            self.cmd_start.emit(run_cmd)
            if self._audit:
                self._audit.log_command_start(run_cmd)

            self.exec_request.emit(run_cmd, timeout_s)

            self._exec_event.clear()
            self._exec_result = None
            # wait a bit longer than the poll-side timeout so it fires first
            fired = self._exec_event.wait(timeout=timeout_s + 60)
            _log.info('exec event fired=%s alive=%s result_len=%s',
                      fired, self._alive,
                      len(self._exec_result) if self._exec_result else 0)

            if not self._alive:
                return

            timed_out = not fired or self._exec_result is None
            output = (
                self._exec_result
                or '[command timed out — if it is still running, send Ctrl+C via the shell bar]'
            )

            if self._audit:
                if timed_out:
                    self._audit.log_error(f'command timed out: {run_cmd}')
                self._audit.log_command_done(output)

            self._do_turn(
                f'Command output:\n```\n{output}\n```\n\nPlease continue.',
                _is_followup=True,
            )
        else:
            _log.info('no RUN/WRITE tag — turn complete')
