#!/usr/bin/env python3
"""Checkpoint live plugin edits, force-kill the server and verify disk recovery."""
import argparse
import re
import time
from pathlib import Path
import tempfile

import java_smoke as wire
from world_api_smoke import ROOT, request, start, stop, wait


def checkpoint(log, process, revision):
    wait(log, process, lambda text: re.search(
        r'World saved[^\n]*revision='+str(revision)+r'(?:\D|$)', text))


def smoke(server, plugin):
    with tempfile.TemporaryDirectory(prefix='sculith checkpoint ') as temporary:
        root = Path(temporary)
        process, log, address, _ = start(server, plugin, root, autosave_interval=1)
        client = None
        try:
            checkpoint(log, process, 0)
            client = wire.Client(address, 'Checkpoint', inventory_fixture=False)
            built = request(client, 'build', {(1, 4, 2): 1, (17, 4, 2): 10})
            assert built['region']['revision'] == 1
            checkpoint(log, process, 1)
            client.socket.close()
            client = None
            # No Ctrl+C, no Stopping hook and no orderly final capture.
            process.kill()
            assert process.wait(timeout=10) != 0
            assert 'Sculith stopped' not in log.read_text(encoding='utf-8')
            before = (root/'region.sculith').read_bytes()

            process, log, address, _ = start(server, plugin, root, autosave_interval=0)
            client = wire.Client(address, 'Recovered', inventory_fixture=False,
                                 edits={(1, 4, 2): 1, (17, 4, 2): 10})
            restored = request(client, 'read', {})
            assert restored['region']['revision'] == 1
            assert [b['block'] for b in restored['blocks']] == [
                'minecraft:stone', 'minecraft:dirt', 'minecraft:air']
            erased = request(client, 'erase', {(1, 4, 2): 0, (17, 4, 2): 0})
            assert erased['region']['revision'] == 2
            time.sleep(1.2)
            assert (root/'region.sculith').read_bytes() == before, 'disabled autosave wrote live edits'
            client.socket.close()
            client = None
            stop(process)
            assert (root/'region.sculith').read_bytes() != before

            process, log, address, _ = start(server, plugin, root, autosave_interval=1)
            client = wire.Client(address, 'FinalSave', inventory_fixture=False)
            restored = request(client, 'read', {})
            assert restored['region']['revision'] == 2
            assert [b['block'] for b in restored['blocks']] == ['minecraft:air'] * 3
            before = (root/'region.sculith').read_bytes()
            time.sleep(1.2)
            assert 'World saved' not in log.read_text(encoding='utf-8'), 'unchanged revision was rewritten'
            assert (root/'region.sculith').read_bytes() == before
            client.socket.close()
            client = None
            stop(process)
        except Exception:
            print(log.read_text(encoding='utf-8'))
            raise
        finally:
            if client:
                client.socket.close()
            if process.poll() is None:
                process.kill()
                process.wait(timeout=10)

    with tempfile.TemporaryDirectory(prefix='sculith checkpoint failure ') as temporary:
        root = Path(temporary)
        process, log, address, _ = start(server, plugin, root, autosave_interval=1)
        client = None
        try:
            checkpoint(log, process, 0)
            client = wire.Client(address, 'SaveFailure', inventory_fixture=False)
            foreign = b'Unrelated operator file: never overwrite this.'
            (root/'region.sculith').write_bytes(foreign)
            assert request(client, 'build')['region']['revision'] == 1
            assert process.wait(timeout=10) != 0, 'failed checkpoint must stop the server'
            assert (root/'region.sculith').read_bytes() == foreign
            text = log.read_text(encoding='utf-8')
            assert 'autosave failed' in text, text
            assert 'final world snapshot was not saved' in text, text
        except Exception:
            print(log.read_text(encoding='utf-8'))
            raise
        finally:
            if client:
                client.socket.close()
            if process.poll() is None:
                process.kill()
                process.wait(timeout=10)
    print('PASS: periodic cross-chunk plugin edits survive forced termination; disabled/unchanged saves, final capture and fail-closed foreign-file protection')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    import os
    suffix = '.exe' if os.name == 'nt' else ''
    parser.add_argument('--server', type=Path, default=ROOT/('target/debug/sculith'+suffix))
    parser.add_argument('--plugin', type=Path, default=ROOT/('target/debug/sculith-world-fixture'+suffix))
    args = parser.parse_args()
    smoke(args.server.resolve(strict=True), args.plugin.resolve(strict=True))
