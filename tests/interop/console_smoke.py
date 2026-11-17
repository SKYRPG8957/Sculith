#!/usr/bin/env python3
"""Piped operator input, live player listing, manual saves and orderly stopping."""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import tempfile
import time

import java_smoke as wire

ROOT = Path(__file__).resolve().parents[2]


def wait(process, log, predicate):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        text = log.read_text(encoding='utf-8')
        result = predicate(text)
        if result:
            return result
        assert process.poll() is None, text
        time.sleep(.02)
    raise TimeoutError(log.read_text(encoding='utf-8'))


def smoke(server, plugin):
    with tempfile.TemporaryDirectory(prefix='sculith console ') as temporary:
        root = Path(temporary)
        settings = root/'server.toml'
        path = root/'nested/region.sculith'
        settings.write_text(
            '[java]\nsessions=true\nrequire_authentication=false\n'
            '[network.java]\nbind="127.0.0.1:0"\n'
            '[network.bedrock]\nenabled=false\nbind="127.0.0.1:0"\n'
            '[world]\nautosave_interval_secs=0\nsnapshot_path='+json.dumps(str(path))+'\n'
            '[[plugins]]\npath='+json.dumps(str(plugin))+'\n', encoding='utf-8')
        for index in range(4):
            log = root/f'{index}.log'
            with log.open('w', encoding='utf-8') as output:
                process = subprocess.Popen(
                    [str(server), 'run', '--config', str(settings), *(['--no-console'] if index == 2 else [])],
                    stdin=subprocess.PIPE, stdout=output, stderr=output, cwd=ROOT,
                    env=dict(os.environ, NO_COLOR='1', RUST_LOG='info'),
                    creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == 'nt' else 0)
            client = None
            def command(value):
                process.stdin.write(value.encode('utf-8')+b'\n')
                process.stdin.flush()
            def stop_signal():
                process.send_signal(signal.CTRL_BREAK_EVENT if os.name == 'nt' else signal.SIGINT)
            try:
                wait(process, log, lambda text: 'Server started' in text)
                address = ('127.0.0.1', int(re.search(r'Java listening.*127\.0\.0\.1:(\d+)', log.read_text())[1]))
                if index == 2:
                    command('stop')
                    time.sleep(.1)
                    assert process.poll() is None, '--no-console unexpectedly consumed stdin'
                    stop_signal()
                    assert process.wait(timeout=10) == 0
                    continue
                client = wire.Client(address, 'ConsoleProbe', edits={(1, 3, 1): 0} if index else None)
                if index == 1:
                    process.stdin.close()
                    # EOF must not stop the server or synthesize any player UI,
                    # messages or pack offers. receive_play consumes peer frames.
                    client.socket.settimeout(.2)
                    try:
                        frame = wire.receive_play(client)
                        raise AssertionError(('unexpected unsolicited frame', frame[0]))
                    except socket.timeout:
                        pass
                    assert process.poll() is None
                    stop_signal()
                    assert process.wait(timeout=10) == 0
                    continue
                command('help')
                wait(process, log, lambda text: 'Commands: help, list, save, stop' in text)
                command('list')
                wait(process, log, lambda text: 'Players (1/20): ConsoleProbe' in text)
                point = (1, 3, 1) if index == 0 else (2, 3, 2)
                wire.send(client.socket, 0x29, wire.vi(0)+wire.pos(*point)+b'\x01'+wire.vi(1))
                client.block(point, 0, 'air')
                if index == 3:
                    foreign = b'operator file: must be preserved'
                    path.write_bytes(foreign)
                    command('save')
                    assert process.wait(timeout=10) != 0
                    assert path.read_bytes() == foreign
                    assert 'autosave failed' in log.read_text(encoding='utf-8')
                else:
                    assert not path.exists(), 'periodic saves should be disabled'
                    command('save')
                    wait(process, log, lambda text: 'World saved' in text)
                    assert path.is_file()
                    command('stop')
                    # Keep stdin open: stop must finish without another read/EOF.
                    assert process.wait(timeout=10) == 0
            except Exception:
                print(log.read_text(encoding='utf-8'))
                raise
            finally:
                if client is not None:
                    client.socket.close()
                if not process.stdin.closed:
                    process.stdin.close()
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=10)
    print('PASS: local help/list/save/stop, no automatic welcome/UI/pack, manual checkpoint recovery, EOF/no-console handling and foreign-file preservation')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    suffix = '.exe' if os.name == 'nt' else ''
    parser.add_argument('--server', type=Path, default=ROOT/('target/debug/sculith'+suffix))
    parser.add_argument('--plugin', type=Path, default=ROOT/('target/debug/sculith-gameplay-plugin'+suffix))
    args = parser.parse_args()
    smoke(args.server.resolve(strict=True), args.plugin.resolve(strict=True))
