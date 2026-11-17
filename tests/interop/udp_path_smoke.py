#!/usr/bin/env python3
"""Linux UDP syscall faults: candidate fallback, ICE deadline and active-path failure."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time

import java_smoke as wire

ROOT = Path(__file__).resolve().parents[2]
FAULT = 'TEST: sendto candidate rejected with EPERM'


def wait(process, log, predicate, seconds=8):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        text = log.read_text(encoding='utf-8')
        result = predicate(text)
        if result:
            return result
        assert process.poll() is None, text
        time.sleep(.02)
    raise TimeoutError(log.read_text(encoding='utf-8'))


def smoke(server, native):
    assert sys.platform == 'linux', 'this syscall fixture requires Linux'
    addresses = json.loads(subprocess.check_output(['ip', '-j', '-4', 'address', 'show']))
    owned = [address['local'] for interface in addresses for address in interface['addr_info']
             if address['scope'] == 'global']
    assert owned, 'candidate fallback fixture needs a non-loopback local IPv4 address'
    # ld.so splits LD_PRELOAD on spaces/colons; keep the shim path unambiguous.
    with tempfile.TemporaryDirectory(prefix='sculith-udp-faults-', dir='/tmp') as temporary:
        root = Path(temporary)
        shim = root / 'sendto.so'
        subprocess.run([
            'cc', '-O2', '-Wall', '-Wextra', '-Werror', '-shared', '-fPIC',
            '-o', str(shim), str(ROOT / 'tests/interop/udp_path_shim.c'), '-ldl'], check=True)
        for case in ['candidate', 'all', 'established']:
            directory = root / case
            directory.mkdir()
            settings = directory / 'server.toml'
            settings.write_text(
                '[java]\nsessions=true\nrequire_authentication=false\n'
                '[bedrock]\nsessions=true\nrequire_authentication=false\n'
                f'login_timeout_secs={3 if case == "all" else 15}\n'
                '[network.java]\nbind="127.0.0.1:0"\n'
                '[network.bedrock]\nbind="127.0.0.1:0"\n', encoding='utf-8')
            gate = directory / 'deny'
            env = dict(os.environ, LD_PRELOAD=str(shim), NO_COLOR='1',
                       SCULITH_TEST_DENY_IP=owned[0] if case == 'candidate' else '*',
                       RUST_LOG='info,sculith::protocol::bedrock::nethernet=debug')
            if case == 'established':
                env['SCULITH_TEST_DENY_FILE'] = str(gate)
            server_log = directory / 'server.log'
            client_log = directory / 'client.log'
            with server_log.open('w', encoding='utf-8') as output:
                process = subprocess.Popen(
                    [str(server), 'run', '--config', str(settings)],
                    cwd=directory, env=env, stdin=subprocess.PIPE, stdout=output, stderr=output)
            probe = java = None
            try:
                wait(process, server_log, lambda text: 'Server started' in text)
                text = server_log.read_text(encoding='utf-8')
                nether = re.search(r'NetherNet listening.*?127\.0\.0\.1:(\d+)', text)[1]
                with client_log.open('w', encoding='utf-8') as output:
                    probe = subprocess.Popen(
                        [str(native), 'http://127.0.0.1:' + nether,
                         *(['--crossplay'] if case == 'established' else [])],
                        cwd=directory, stdout=output, stderr=output,
                        env=dict(os.environ, SCULITH_TEST_LOOPBACK_CANDIDATE='1'))
                if case == 'candidate':
                    assert probe.wait(timeout=30) == 0, client_log.read_text(encoding='utf-8')
                    text = server_log.read_text(encoding='utf-8')
                    assert FAULT in text and 'continuing ICE operation="send"' in text, text
                    assert 'Bedrock client acknowledged the flat-world spawn' in text, text
                    assert 'PASS: independent gophertunnel NetherNet world spawn' in client_log.read_text(encoding='utf-8')
                elif case == 'all':
                    wait(process, server_log, lambda text: 'NetherNet ICE/DTLS/SCTP establishment failed' in text)
                    text = server_log.read_text(encoding='utf-8')
                    assert FAULT in text and 'WebRTC transport connected' not in text, text
                    # The bounded server deadline fires before the client's 20s dial deadline.
                    assert probe.poll() is None, client_log.read_text(encoding='utf-8')
                else:
                    wait(probe, client_log, lambda text: 'CROSSPLAY: committed [0 3 2] minecraft:air' in text)
                    tcp = int(re.search(r'Java listening.*?127\.0\.0\.1:(\d+)', text)[1])
                    java = wire.Client(('127.0.0.1', tcp), 'UdpFaultJava',
                                       edits={(0, 3, 2): 0}, inventory_fixture=False)
                    gate.touch()
                    wire.send(java.socket, 0x29, wire.vi(0) + wire.pos(1, 3, 1) + b'\x01' + wire.vi(1))
                    java.block((1, 3, 1), 0, 'air')
                    wait(process, server_log, lambda text: re.search(
                        r'NetherNet session ended.*Operation not permitted', text))
                    # Permission loss on the active path must remain fatal to that
                    # session. The world owner and the other edition stay available.
                    wire.send(java.socket, 0x29, wire.vi(0) + wire.pos(2, 3, 2) + b'\x01' + wire.vi(2))
                    java.block((2, 3, 2), 0, 'air')
                    assert FAULT in server_log.read_text(encoding='utf-8')
                process.stdin.write(b'stop\n')
                process.stdin.flush()
                assert process.wait(timeout=10) == 0
            except Exception:
                print(server_log.read_text(encoding='utf-8'))
                if client_log.exists():
                    print(client_log.read_text(encoding='utf-8'))
                raise
            finally:
                if java is not None:
                    java.socket.close()
                if probe is not None and probe.poll() is None:
                    probe.kill()
                    probe.wait(timeout=5)
                process.stdin.close()
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=10)
    print('PASS: denied UDP candidate falls back to healthy path; all-denied ICE reaches its deadline; active-path permission loss closes only that session')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--server', type=Path, default=ROOT / 'target/debug/sculith')
    parser.add_argument('--client', type=Path, required=True)
    args = parser.parse_args()
    smoke(args.server.resolve(strict=True), args.client.resolve(strict=True))
