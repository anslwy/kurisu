#!/usr/bin/env python3
"""Inject a post-promotion recycle timeout in an isolated home, never a live deploy."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('binary', type=Path)
parser.add_argument('--tmpdir', required=True, type=Path)
args = parser.parse_args()
binary = args.binary.resolve()

with tempfile.TemporaryDirectory(prefix='recycle-failure-', dir=args.tmpdir) as temporary:
    root = Path(temporary)
    target = root / 'installed-ku'
    shutil.copy2(binary, target)
    # ELF accepts trailing bytes. A same-commit old executable must still be
    # replaced; comparing version strings alone cannot detect this stale target.
    with target.open('ab') as stream:
        stream.write(b'previous installed executable\n')
    old_digest = hashlib.sha256(target.read_bytes()).hexdigest()
    new_digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    assert old_digest != new_digest
    env = dict(os.environ, KURISU_HOME=str(root), TMPDIR=str(args.tmpdir.resolve()),
        KURISU_TEST_TMPDIR=str(args.tmpdir.resolve()), KURISU_NO_RESUME='1')

    def request(op):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(20)
            client.connect(str(root / 'kurisu.sock'))
            client.sendall((json.dumps(dict(op=op)) + '\n').encode())
            reply = json.loads(client.makefile().readline())
            assert 'error' not in reply, reply
            return reply['ok']

    with (root / 'daemon.log').open('w') as log:
        old = subprocess.Popen([str(target), 'serve'], env=env, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 15
            while not (root / 'kurisu.sock').exists():
                assert old.poll() is None, (root / 'daemon.log').read_text()
                assert time.monotonic() < deadline, 'isolated daemon startup timeout'
                time.sleep(.02)
            before_pid = request('ping')['pid']
            result = subprocess.run([str(binary), 'source', 'install', '--from', str(binary),
                '--target', str(target), '--yes'], env=dict(env, KURISU_DEPLOY_FAULT='web_recycle'),
                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=100)
            assert result.returncode != 0, result.stdout
            assert 'fault injection: standalone /web recycle timed out' in result.stdout, result.stdout
            assert 'daemon and installed binary match' in result.stdout, result.stdout
            assert 'frontend requires repair' in result.stdout, result.stdout
            assert 'old binary path remains intact' not in result.stdout, result.stdout
            after = request('deploy_binary_sha256')
            assert after['pid'] != before_pid
            assert after['sha256'] == new_digest
            assert hashlib.sha256(target.read_bytes()).hexdigest() == new_digest
            assert target.stat().st_mode & 0o777 == 0o755
            with open(f'/proc/{after["pid"]}/exe', 'rb') as executable:
                assert hashlib.file_digest(executable, 'sha256').hexdigest() == new_digest
            print(f'PASS recycle timeout: new target bytes; installed=daemon SHA256 {new_digest}; explicit frontend failure', flush=True)
        finally:
            try:
                request('shutdown')
            except (OSError, AssertionError, ValueError):
                pass
            try:
                old.wait(timeout=10)
            except subprocess.TimeoutExpired:
                old.kill()
                old.wait()
