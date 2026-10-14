#!/usr/bin/env python3
"""Private, bounded real-PTY driver. No network, credential or provider access."""
import argparse
import codecs
import errno
import fcntl
import hashlib
import json
import os
import pathlib
import pty
import select
import signal
import struct
import subprocess
import termios
import time
import unicodedata


class Screen:
    """Normalize the selected backend's VT cursor/erase output into fixed rows."""
    def __init__(self, width, height):
        self.resize(width, height)
        self.decoder = codecs.getincrementaldecoder('utf-8')('strict')
        self.pending = ''
        self.alternate = False
        self.cursor_visible = True
        self.replies = []

    def resize(self, width, height):
        self.width, self.height = width, height
        self.rows = [[' '] * width for _ in range(height)]
        self.x = self.y = 0

    def feed(self, raw):
        self.pending += self.decoder.decode(raw)
        while self.pending:
            if self.pending.startswith('\x1b'):
                if len(self.pending) < 2:
                    return
                if self.pending[1] != '[':
                    raise AssertionError('unexpected non-CSI terminal escape')
                end = next((i for i in range(2, len(self.pending))
                            if '@' <= self.pending[i] <= '~'), None)
                if end is None:
                    return
                args, command = self.pending[2:end], self.pending[end]
                self.pending = self.pending[end + 1:]
                private = args.startswith('?')
                values = [int(v) if v else 0 for v in args.lstrip('?').split(';')]
                n = values[0] or 1
                if private and command in 'hl':
                    if 1049 in values:
                        self.alternate = command == 'h'
                    if 25 in values:
                        self.cursor_visible = command == 'h'
                elif command in 'Hf':
                    self.y, self.x = n - 1, (values[1] or 1) - 1 if len(values) > 1 else 0
                elif command == 'G':
                    self.x = n - 1
                elif command == 'A':
                    self.y = max(0, self.y - n)
                elif command == 'B':
                    self.y = min(self.height - 1, self.y + n)
                elif command == 'C':
                    self.x += n
                elif command == 'D':
                    self.x = max(0, self.x - n)
                elif command == 'J':
                    if values[0] in (2, 3):
                        self.rows = [[' '] * self.width for _ in range(self.height)]
                    elif values[0] == 0:
                        for y in range(self.y, self.height):
                            for x in range(self.x if y == self.y else 0, self.width):
                                self.rows[y][x] = ' '
                    elif values[0] == 1:
                        for y in range(self.y + 1):
                            for x in range(self.x + 1 if y == self.y else self.width):
                                self.rows[y][x] = ' '
                    else:
                        raise AssertionError('invalid erase mode')
                elif command == 'K':
                    start, stop = (0, self.width) if values[0] == 2 else (self.x, self.width)
                    for x in range(start, stop):
                        self.rows[self.y][x] = ' '
                elif command == 'm' or (private and command in 'hl'):
                    pass
                elif command == 'n' and values == [6]:
                    self.replies.append(f'\x1b[{self.y + 1};{self.x + 1}R'.encode())
                else:
                    raise AssertionError(f'unhandled CSI {args}{command}')
                continue
            char, self.pending = self.pending[0], self.pending[1:]
            if char == '\r':
                self.x = 0
            elif char == '\n':
                self.y = min(self.height - 1, self.y + 1)
            elif unicodedata.combining(char):
                if self.x:
                    self.rows[self.y][min(self.x - 1, self.width - 1)] += char
            elif ord(char) >= 32:
                width = 2 if unicodedata.east_asian_width(char) in ('W', 'F') else 1
                if self.y < self.height and self.x < self.width:
                    self.rows[self.y][self.x] = char
                    if width == 2 and self.x + 1 < self.width:
                        self.rows[self.y][self.x + 1] = ''
                self.x += width
            else:
                raise AssertionError('unexpected terminal control')

    def text(self):
        return '\n'.join(''.join(row).rstrip() for row in self.rows)


def run(binary, root, output, config):
    output.mkdir(parents=True, exist_ok=True)
    os.chmod(output, 0o700)
    master, slave = pty.openpty()
    initial = termios.tcgetattr(slave)
    width, height = config.get('size', [240, 24])
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', height, width, 0, 0))
    screen = Screen(width, height)
    raw = bytearray()
    frames = []
    proc = None
    status = None
    failure = None
    attributes_restored = False
    final_attributes = None
    deadline = time.monotonic() + 20

    def child_setup():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)
        os.tcsetpgrp(0, os.getpgrp())

    def pump(duration=0.15):
        until = min(deadline, time.monotonic() + duration)
        while time.monotonic() < until:
            if not select.select([master], [], [], min(0.03, max(0, until - time.monotonic())))[0]:
                continue
            try:
                data = os.read(master, 65536)
            except OSError as exc:
                if exc.errno == errno.EIO:
                    break
                raise
            if not data:
                break
            raw.extend(data)
            (output / 'private-output.bin').write_bytes(raw)
            if len(raw) > 4 * 1024 * 1024:
                raise AssertionError('PTY output bound exceeded')
            screen.feed(data)
            for reply in screen.replies:
                os.write(master, reply)
            screen.replies.clear()

    def expect(text):
        limit = min(deadline, time.monotonic() + 4)
        while text not in screen.text() and time.monotonic() < limit:
            pump()
            if proc.poll() is not None:
                break
        assert text in screen.text(), f'missing frame marker: {text!r}'
        frames.append({'marker': text, 'size': [screen.width, screen.height], 'rows': screen.text()})

    try:
        env = {'TERM': 'xterm-256color', 'LANG': 'en_US.UTF-8', 'LC_ALL': 'en_US.UTF-8',
               'HOME': str(root), 'TMPDIR': str(root)}
        proc = subprocess.Popen([str(binary), *config['args']], cwd=root, env=env,
                                stdin=slave, stdout=slave, stderr=slave, preexec_fn=child_setup)
        for action in config['actions']:
            if 'expect' in action:
                expect(action['expect'])
            elif 'keys' in action:
                os.write(master, action['keys'].encode('utf-8'))
                pump()
            elif 'paste' in action:
                os.write(master, ('\x1b[200~' + action['paste'] + '\x1b[201~').encode('utf-8'))
                pump()
            elif 'resize' in action:
                width, height = action['resize']
                screen.resize(width, height)
                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', height, width, 0, 0))
                os.kill(proc.pid, signal.SIGWINCH)
                pump()
            elif 'signal' in action:
                os.kill(proc.pid, getattr(signal, action['signal']))
                pump()
            elif 'replace_file' in action:
                (root / 'source.md').write_text(action['replace_file'])
            elif 'release_remote' in action:
                (root / 'remote-ready').touch()
            elif 'make_recovery_unwritable' in action:
                # Deterministic filesystem failure, even under privileged test users.
                recovery = root / 'cache.sqlite.tui-recovery' / 'draft.json'
                if recovery.exists():
                    recovery.unlink()
                recovery.mkdir()
            else:
                raise AssertionError('unknown recorded action')
        while proc.poll() is None and time.monotonic() < deadline:
            pump()
        assert proc.poll() is not None, 'child exceeded 20-second hard bound'
        pump()
        status = proc.wait()
        assert status == config.get('exit', 0), f'unexpected exit {status}'
        attributes_restored = termios.tcgetattr(master) == initial
        assert attributes_restored, 'terminal attributes not restored'
        if config.get('interactive', True):
            assert b'\x1b[?1049h' in raw and b'\x1b[?1049l' in raw
            assert b'\x1b[?2004h' in raw and b'\x1b[?2004l' in raw
            assert screen.cursor_visible and not screen.alternate
    except BaseException as exc:
        failure = str(exc)
    finally:
        if proc is not None:
            if proc.poll() is None:
                proc.terminate()
                try:
                    pump(0.3)
                    proc.wait(timeout=4)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    # Closing the controller's terminal handles unblocks Darwin
                    # terminal IO before the forced child is reaped.
                    os.close(master)
                    os.close(slave)
                    master = slave = None
                    proc.wait(timeout=10)
            status = proc.returncode
        if master is not None:
            pump(0.1)
            final_attributes = termios.tcgetattr(master)
            attributes_restored = final_attributes == initial
            os.close(master)
            os.close(slave)
        (output / 'private-output.bin').write_bytes(raw)
        os.chmod(output / 'private-output.bin', 0o600)
        (output / 'frames.json').write_text(json.dumps(frames, ensure_ascii=False, indent=2) + '\n')
        (output / 'last-frame.txt').write_text(screen.text() + '\n')
        (output / 'result.json').write_text(json.dumps({
            'exit': status, 'failure': failure, 'reaped': proc is None or proc.poll() is not None,
            'terminalAttributesRestored': attributes_restored,
            'finalTermios': final_attributes,
            'rawSha256': hashlib.sha256(raw).hexdigest(), 'initialTermios': initial,
            'terminal': 'xterm-256color', 'dimensions': config.get('size', [240, 24]),
            'frameCount': len(frames), 'environment': 'allowlist; no credentials/network',
        }, default=lambda value: list(value), indent=2) + '\n')
    if failure:
        raise AssertionError(failure)


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    for name in ('binary', 'root', 'output', 'fixture'):
        parser.add_argument('--' + name, required=True)
    args = parser.parse_args()
    run(pathlib.Path(args.binary), pathlib.Path(args.root), pathlib.Path(args.output),
        json.loads(pathlib.Path(args.fixture).read_text()))
