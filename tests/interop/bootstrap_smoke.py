#!/usr/bin/env python3
"""First-run configuration, local Java/native entry and restart without plugins."""
import argparse
import hashlib
import os
from pathlib import Path
import re
import socket
import subprocess
import tempfile
import time
import tomllib

import java_smoke as wire

ROOT = Path(__file__).resolve().parents[2]
TEMPLATE = ROOT / 'crates/sculith-server/src/config/default.toml'


def wait(process, log, predicate):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        text = log.read_text(encoding='utf-8')
        result = predicate(text)
        if result:
            return result
        assert process.poll() is None, text
        time.sleep(.02)
    raise TimeoutError(log.read_text(encoding='utf-8'))


def smoke(server, native):
    env = dict(os.environ, NO_COLOR='1', RUST_LOG='info')
    with tempfile.TemporaryDirectory(prefix='sculith first run ') as temporary:
        root = Path(temporary)

        def cli(*args, ok=True):
            result = subprocess.run(
                [str(server), *args], cwd=root, env=env,
                capture_output=True, text=True, encoding='utf-8', timeout=10)
            assert (result.returncode == 0) == ok, result.stdout + result.stderr
            return result.stdout + result.stderr

        # Configuration checks and metadata inspection never initialize a server.
        assert 'shared-world-preview' in cli('check')
        cli('versions')
        cli('run', '--offline', '--config', 'absent.toml', ok=False)
        cli('check', '--config', 'absent.toml', ok=False)
        assert not list(root.iterdir())

        settings = root / 'sculith.toml'
        original = b'operator file: invalid TOML'
        settings.write_bytes(original)
        cli('run', '--offline', ok=False)
        assert settings.read_bytes() == original
        settings.unlink()
        settings.mkdir()
        cli('run', '--offline', ok=False)
        assert settings.is_dir()
        settings.rmdir()
        if os.name != 'nt':
            settings.symlink_to(root / 'absent.toml')
            cli('check', ok=False)
            cli('run', '--offline', ok=False)
            assert settings.is_symlink() and not (root / 'absent.toml').exists()
            settings.unlink()

        # The CLI override is validated before opening any listener or key file.
        for address in ['127.0.0.1:25565', '127.0.0.1:19132']:
            public = TEMPLATE.read_text(encoding='utf-8').replace(address, address.replace('127.0.0.1', '0.0.0.0'))
            settings.write_text(public, encoding='utf-8')
            cli('run', '--offline', ok=False)
            assert settings.read_text(encoding='utf-8') == public
            assert [entry.name for entry in root.iterdir()] == ['sculith.toml']
            settings.unlink()

        snapshot = root / 'world/region.sculith'
        key = root / 'world/nethernet-identity.pem'
        config_hash = key_hash = snapshot_hash = None
        for attempt in range(2):
            log = root / f'server-{attempt}.log'
            with log.open('w', encoding='utf-8') as output:
                process = subprocess.Popen(
                    [str(server), 'run', '--offline'], cwd=root, env=env,
                    stdin=subprocess.PIPE, stdout=output, stderr=output)
            java = None

            def command(value):
                process.stdin.write(value.encode('utf-8') + b'\n')
                process.stdin.flush()

            try:
                wait(process, log, lambda text: 'Server started' in text)
                text = log.read_text(encoding='utf-8')
                assert ('Created server configuration' in text) == (attempt == 0), text
                parsed = tomllib.loads(settings.read_text(encoding='utf-8'))
                assert parsed['java']['sessions'] and parsed['bedrock']['sessions']
                assert parsed['java']['require_authentication'] and parsed['bedrock']['require_authentication']
                assert parsed['world']['snapshot_path'] == 'world/region.sculith'
                assert parsed['world']['autosave_interval_secs'] == 60
                assert not parsed.get('plugins') and 'content' not in parsed
                digest = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
                if attempt == 0:
                    assert settings.read_text(encoding='utf-8') == TEMPLATE.read_text(encoding='utf-8')
                    config_hash, key_hash = digest(settings), digest(key)
                    native_port = re.search(r'NetherNet listening.*?127\.0\.0\.1:(\d+)', text)[1]
                    result = subprocess.run(
                        [str(native), 'http://127.0.0.1:' + native_port],
                        capture_output=True, text=True, encoding='utf-8', timeout=40)
                    assert result.returncode == 0, result.stdout + result.stderr
                    assert 'PASS: independent gophertunnel NetherNet world spawn' in result.stdout
                assert digest(settings) == config_hash and digest(key) == key_hash
                java_port = int(re.search(r'Java listening.*?127\.0\.0\.1:(\d+)', text)[1])
                java = wire.Client(
                    ('127.0.0.1', java_port), 'BootstrapJava',
                    edits={(1, 3, 1): 0} if attempt else None, inventory_fixture=False)
                if attempt == 0:
                    wire.send(java.socket, 0x29, wire.vi(0) + wire.pos(1, 3, 1) + b'\x01' + wire.vi(1))
                    java.block((1, 3, 1), 0, 'air')
                    command('save')
                    wait(process, log, lambda text: 'World saved' in text)
                    snapshot_hash = digest(snapshot)
                else:
                    # No welcome, unsolicited UI or shared-pack offer on normal entry.
                    java.socket.settimeout(.2)
                    try:
                        unexpected = wire.receive_play(java)
                        raise AssertionError(('unsolicited frame', unexpected[0]))
                    except socket.timeout:
                        pass
                command('stop')
                # Keep stdin open: stopping cannot depend on EOF or another line.
                assert process.wait(timeout=15) == 0
                assert digest(settings) == config_hash and digest(key) == key_hash
                assert digest(snapshot) == snapshot_hash
            except Exception:
                print(log.read_text(encoding='utf-8'))
                raise
            finally:
                if java is not None:
                    java.socket.close()
                process.stdin.close()
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=10)
    print('PASS: read-only check, missing/invalid config preservation, local-only offline override, plugin-free Java/native first run, stable config/key and saved-world restart')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    suffix = '.exe' if os.name == 'nt' else ''
    parser.add_argument('--server', type=Path, default=ROOT / ('target/debug/sculith' + suffix))
    parser.add_argument('--client', type=Path, required=True)
    args = parser.parse_args()
    smoke(args.server.resolve(strict=True), args.client.resolve(strict=True))
